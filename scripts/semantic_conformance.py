#!/usr/bin/env python3
"""Real compiler differential conformance. Tool/protocol errors never mean agreement."""
import argparse
from collections import Counter
from collections.abc import Mapping
import hashlib
import gzip
import base64
import io
import json
import os
from pathlib import Path
import queue
import resource
import re
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time

from semantic_generate import PROFILE, canonical, generate, layout_variant, remap_answer, validate_bounds
from semantic_install import ROOT, SOURCE, digest, file_digest, install, native_worker_command, runtime_path
from semantic_process import signal_group

SCHEMA=1
MAX_OUTPUT=4_000_000
GATE_PROFILE=json.loads((ROOT/'scripts/semantic_gate_profile.json').read_text())
SHUTDOWN_SECONDS=GATE_PROFILE['shutdown_seconds']
sys.modules.setdefault('semantic_conformance',sys.modules[__name__])


class ToolFailure(RuntimeError):
    pass


class Worker:
    """One in-flight JSONL reply; bounded pipes and unconditional group cleanup."""
    def __init__(self,command,environment=None):
        self.home=tempfile.TemporaryDirectory(prefix='bend-semantic-home-')
        env={key:value for key,value in os.environ.items() if key in ('PATH','SYSTEMROOT','TMPDIR','TMP','TEMP')}
        env.update({'HOME':self.home.name,'BEND_LIB':self.home.name+'/lib','BEND_HUB':'http://127.0.0.1:1'})
        if environment:env.update(environment)
        try:self.process=subprocess.Popen(command,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env,cwd=self.home.name,start_new_session=True)
        except BaseException:self.home.cleanup();raise
        self.lines=queue.Queue(maxsize=1);self.errors=bytearray();self.last_raw=b''
        self.lock=threading.RLock();self.awaiting=False;self.failure=None;self.closed=False
        os.set_blocking(self.process.stdin.fileno(),False)
        self.reader=threading.Thread(target=self._read,daemon=True);self.reader.start()
        self.err_reader=threading.Thread(target=self._stderr,daemon=True);self.err_reader.start()

    def _fail(self,message):
        with self.lock:
            if self.closed:return
            if self.failure is None:self.failure=ToolFailure(message)
            try:self.lines.put_nowait(self.failure)
            except queue.Full:pass
        signal_group(self.process,signal.SIGKILL)

    def _read(self):
        pending=bytearray()
        try:
            while True:
                chunk=os.read(self.process.stdout.fileno(),65536)
                if not chunk:
                    self._fail('truncated-reply' if pending else 'unexpected-eof');return
                with self.lock:
                    if not self.awaiting:self._fail('unsolicited-reply');return
                pending.extend(chunk)
                self.last_raw=bytes(pending[:MAX_OUTPUT])
                if len(pending)>MAX_OUTPUT:self._fail('output-limit');return
                if b'\n' in pending:
                    line,_,rest=pending.partition(b'\n')
                    if rest:self._fail('multiple-or-unsolicited-replies');return
                    with self.lock:
                        if not self.awaiting:self._fail('unsolicited-reply');return
                        self.awaiting=False
                        try:self.lines.put_nowait((bytes(line),time.monotonic()))
                        except queue.Full:self._fail('queued-output-limit');return
                    pending.clear()
        except OSError as error:self._fail(str(error))

    def _stderr(self):
        try:
            while True:
                chunk=os.read(self.process.stderr.fileno(),16384)
                if not chunk:return
                self.errors.extend(chunk)
                if len(self.errors)>MAX_OUTPUT:
                    del self.errors[MAX_OUTPUT:];self._fail('stderr-output-limit');return
        except OSError as error:self._fail(str(error))

    def start(self,request,timeout):
        deadline=time.monotonic()+timeout
        if timeout<=0:raise ToolFailure('deadline-exhausted')
        raw=canonical(request)
        if len(raw)>300000:raise ToolFailure('input-limit')
        with self.lock:
            if self.failure:raise self.failure
            if self.awaiting or not self.lines.empty():raise ToolFailure('overlapping-or-unsolicited-reply')
            self.last_raw=b''
            self.awaiting=True
        remaining=memoryview(raw+b'\n')
        try:
            while remaining:
                seconds=deadline-time.monotonic()
                if seconds<=0:self._fail('worker-write-timeout');raise self.failure
                _,writable,_=select.select([],[self.process.stdin.fileno()],[],seconds)
                if not writable:self._fail('worker-write-timeout');raise self.failure
                try:written=os.write(self.process.stdin.fileno(),remaining)
                except BlockingIOError:continue
                remaining=remaining[written:]
        except (BrokenPipeError,OSError) as error:
            self._fail('worker-crash');raise self.failure from error
        return request,deadline

    def receive(self,pending):
        request,deadline=pending
        try:packet=self.lines.get(timeout=max(0,deadline-time.monotonic()))
        except queue.Empty as error:
            self._fail('worker-timeout');raise self.failure from error
        with self.lock:
            if self.failure:raise self.failure
        if isinstance(packet,Exception):raise packet
        line,received_at=packet
        # Independent work may delay consumption, but never extend the reply deadline.
        if received_at>deadline:
            self._fail('worker-timeout');raise self.failure
        self.last_raw=line
        try:reply=json.loads(line,parse_constant=lambda value:float('invalid-'+value))
        except (ValueError,UnicodeError) as error:self._fail('malformed-reply');raise ToolFailure('malformed-reply') from error
        if not isinstance(reply,dict) or reply.get('schema')!=SCHEMA or reply.get('id')!=request['id'] or reply.get('root')!=request['root']:
            self._fail('stale-or-incomplete-reply');raise ToolFailure('stale-or-incomplete-reply')
        if reply.get('outcome') not in ('valid','compiler-error','oracle-error'):
            self._fail('missing-outcome');raise ToolFailure('missing-outcome')
        return reply

    def ask(self,request,timeout):
        return self.receive(self.start(request,timeout))


    def close(self,deadline=None):
        deadline=min(time.monotonic()+SHUTDOWN_SECONDS,deadline) if deadline is not None else time.monotonic()+SHUTDOWN_SECONDS
        with self.lock:
            if getattr(self,'cleanup_complete',False):return
            self.closed=True
        errors=[]
        # All waits share the pinned grace, rather than each receiving a new
        # timeout. Always escalate even if the group leader already exited.
        try:signal_group(self.process,signal.SIGTERM)
        except OSError as error:errors.append(str(error))
        try:self.process.wait(timeout=max(0,min(1,deadline-time.monotonic())))
        except subprocess.TimeoutExpired:pass
        try:signal_group(self.process,signal.SIGKILL)
        except OSError as error:errors.append(str(error))
        try:self.process.wait(timeout=max(0,deadline-time.monotonic()))
        except subprocess.TimeoutExpired:errors.append('worker-reap-timeout')
        for reader in (self.reader,self.err_reader):
            reader.join(timeout=max(0,deadline-time.monotonic()))
            if reader.is_alive():errors.append('worker-reader-shutdown-timeout')
        for stream in (self.process.stdin,self.process.stdout,self.process.stderr):stream.close()
        self.home.cleanup()
        if errors:raise ToolFailure(';'.join(errors))
        self.cleanup_complete=True




def open_workers(compiler_command,candidate_command):
    compiler=Worker(compiler_command)
    try:probe=Worker(candidate_command)
    except BaseException:compiler.close();raise
    return compiler,probe


class RawHistorySpool:
    """Visible, artifact-budgeted compressed bytes; only bounded indexes in RAM."""
    def __init__(self,directory):
        self.directory=Path(directory);self.directory.mkdir(parents=True,exist_ok=True)
        self.file=tempfile.NamedTemporaryFile(mode='w+b',prefix='oracle-history-',suffix='.gz',dir=self.directory,delete=False)
        self.path=Path(self.file.name);self.lock=threading.Lock()
        self.index={};self.histories={'forward':[],'reverse':[]};self.items={'forward':[],'reverse':[]};self.closed=False
        try:self._budget(0)
        except BaseException:self.close();raise

    def _budget(self,additional):
        size=additional;count=0
        for path in self.directory.rglob('*'):
            if path.is_symlink():raise ToolFailure('artifact-symlink')
            if path.is_file():
                count+=1;size+=path.stat().st_size
                if size>GATE_PROFILE['artifact_bytes'] or count>GATE_PROFILE['artifact_files']:
                    raise ToolFailure('history-spool-artifact-limit')

    def append(self,history,item,raw,fingerprint):
        if len(raw)>MAX_OUTPUT:raise ToolFailure('history-spool-reply-limit')
        try:compressed=gzip.compress(raw,compresslevel=1,mtime=0)
        except (OSError,MemoryError,ValueError) as error:raise ToolFailure('history-spool-compression:'+str(error)) from error
        with self.lock:
            if self.closed:raise ToolFailure('closed-history-spool')
            key=(history,item['id'])
            if key in self.index:raise ToolFailure('duplicate-history-spool-view')
            self._budget(len(compressed))
            self.file.seek(0,os.SEEK_END);offset=self.file.tell()
            self.file.write(compressed);self.file.flush()
            raw_sha=digest(raw)
            self.index[key]=(offset,len(compressed),len(raw),raw_sha)
            self.histories[history].append({'id':item['id'],'oracle_sha256':fingerprint,'raw_sha256':raw_sha,'input_sha256':item['input_sha256'] if 'input_sha256' in item else digest(canonical(request_for(item)))})
            self.items[history].append(item)

    def load(self,history,key):
        with self.lock:
            offset,length,raw_length,raw_sha=self.index[(history,key)]
            self.file.seek(offset);compressed=self.file.read(length)
        with gzip.GzipFile(fileobj=io.BytesIO(compressed)) as stream:raw=stream.read(MAX_OUTPUT+1)
        if len(raw)!=raw_length or digest(raw)!=raw_sha:raise ToolFailure('corrupt-history-spool')
        return raw

    def prior(self,history,key):
        rows=self.histories[history]
        position=next(index for index,row in enumerate(rows) if row['id']==key)
        return rows[:position]

    def requests(self,history,key):
        rows=self.histories[history]
        position=next(index for index,row in enumerate(rows) if row['id']==key)
        return [request_for(item) for item in self.items[history][:position+1]]

    def close(self,deadline=None,retain=False):
        deadline=time.monotonic()+SHUTDOWN_SECONDS if deadline is None else deadline
        if not self.lock.acquire(timeout=max(0,deadline-time.monotonic())):
            raise ToolFailure('history-spool-shutdown-timeout')
        try:
            if self.closed:return
            self.closed=True
            self.file.close()
            if not retain:self.path.unlink()
        finally:self.lock.release()


class SpoolAnswers(Mapping):
    """Load authenticated replies without retaining every decoded response."""
    def __init__(self,spool,history):
        self.spool=spool;self.history=history

    def __getitem__(self,key):
        return json.loads(self.spool.load(self.history,key))

    def __iter__(self):
        return (row['id'] for row in self.spool.histories[self.history])

    def __len__(self):
        return len(self.spool.histories[self.history])


class ReverseHistory:
    """One independently owned persistent oracle; retain hashes, not native Books."""
    def __init__(self,command,items,provenance,deadline,origin,spool):
        self.worker=Worker(command)
        self.items=list(reversed(items));self.provenance=provenance
        self.deadline=deadline;self.origin=origin;self.spool=spool;self.cleanup_deadline=None
        self.cancelled=threading.Event();self.failure=None;self.cleanup_error=None;self.cancel_error=None
        self.completed=[];self.metrics={'pid':self.worker.process.pid,'planned':len(self.items),'cases':[]}
        self.thread=threading.Thread(target=self._run,name='semantic-reverse-history',daemon=True)
        try:self.thread.start()
        except BaseException:self.worker.close();raise

    def _run(self):
        self.metrics['started_seconds']=time.monotonic()-self.origin
        item=None;reply={}
        try:
            for item in self.items:
                if self.cancelled.is_set():return
                reply={};started=time.monotonic()
                reply=self.worker.ask(request_for(item),min(5 if item['case_id'].startswith('generated-') else 15,self.deadline-started))
                ended=time.monotonic()
                if reply.get('provenance')!=self.provenance:raise ToolFailure('worker-pin-mismatch')
                if reply.get('outcome')=='oracle-error':raise ToolFailure('oracle-error:'+reply.get('error',''))
                fingerprint=digest(canonical(normalized(reply)))
                self.spool.append('reverse',item,self.worker.last_raw,fingerprint)
                self.completed.append({'id':item['id'],'oracle_sha256':fingerprint})
                self.metrics['cases'].append({'id':item['id'],'oracle_rpc_started_seconds':started-self.origin,'oracle_rpc_seconds':ended-started,'oracle_stages':reply.get('metrics',{})})
            self.metrics['complete']=True
        except BaseException as error:
            # Only our explicit cancellation is suppressible, never an original
            # EOF, timeout, parity failure, or malformed reply racing with it.
            if not (self.cancelled.is_set() and str(error)=='history-cancelled'):
                self.failure={'error':error,'item':item,'oracle':reply,'raw':self.worker.last_raw}
        finally:
            self.metrics['finished_seconds']=time.monotonic()-self.origin
            self.metrics['completed']=len(self.completed)
            cutoff=min(self.deadline+SHUTDOWN_SECONDS,self.cleanup_deadline if self.cleanup_deadline is not None else time.monotonic()+SHUTDOWN_SECONDS)
            try:self.worker.close(deadline=cutoff)
            except BaseException as error:self.cleanup_error=error
            if self.failure is None and self.cancelled.is_set() and item is not None:
                # A native error can already be in the pipe when forward
                # failure cancels ask. After readers drain, retain that original
                # reply rather than replacing it with our cancellation marker.
                raw=self.worker.last_raw
                try:queued=json.loads(raw)
                except (ValueError,UnicodeError):queued=None
                if isinstance(queued,dict) and queued.get('schema')==SCHEMA and queued.get('id')==item['id'] and queued.get('root')==item['root'] and queued.get('outcome')=='oracle-error':
                    self.failure={'error':ToolFailure('oracle-error:'+str(queued.get('error',''))),'item':item,'oracle':queued,'raw':raw}
            self.metrics['closed_seconds']=time.monotonic()-self.origin

    def check(self):
        if self.failure:
            failure=self.failure
            error=ToolFailure(str(failure['error']))
            error.history_failure={**failure,'stderr':self.worker.errors.decode(errors='replace')}
            raise error from failure['error']
        if self.cleanup_error:raise ToolFailure('reverse-history-cleanup:'+str(self.cleanup_error))

    def cancel(self,deadline=None):
        cutoff=min(self.deadline+SHUTDOWN_SECONDS,time.monotonic()+SHUTDOWN_SECONDS) if deadline is None else min(self.deadline+SHUTDOWN_SECONDS,deadline)
        self.cleanup_deadline=min(self.cleanup_deadline,cutoff) if self.cleanup_deadline is not None else cutoff
        self.cancelled.set()
        if self.thread.is_alive():
            try:self.worker._fail('history-cancelled')
            except BaseException as error:self.cancel_error=error

    def close(self,deadline=None):
        self.cancel(deadline)
        # Killing the owned group also releases ask's queue/write wait. The
        # thread alone closes streams, avoiding a close-versus-select race.
        self.thread.join(timeout=max(0,self.cleanup_deadline-time.monotonic()))
        if self.thread.is_alive():raise ToolFailure('reverse-history-shutdown-timeout')
        if self.cleanup_error:raise ToolFailure('reverse-history-cleanup:'+str(self.cleanup_error))
        if self.cancel_error:raise ToolFailure('reverse-history-cancel:'+str(self.cancel_error))

    def finish(self,forward):
        self.thread.join(timeout=max(0,self.deadline-time.monotonic()))
        if self.thread.is_alive():
            self.close()
            self.check()
            raise ToolFailure('deadline-exhausted-during-reverse-history')
        self.check()
        if not self.metrics.get('complete') or len(self.completed)!=len(self.items):
            raise ToolFailure('incomplete-reverse-history')
        if any(row['id']!=item['id'] or item['id'] not in forward for item,row in zip(self.items,self.completed)):
            raise ToolFailure('incomplete-forward-history')
        for evidence in self.divergences(forward):
            error=evidence['error'];error.history_failure=evidence
            raise error
        return self.completed

    def divergences(self,forward,pending=None):
        if self.thread.is_alive():raise ToolFailure('unsettled-reverse-history')
        for item,row in zip(self.items,self.completed):
            key=item['id']
            pending_forward=pending is not None and pending['item']['id']==key
            forward_sha=pending['oracle_sha256'] if pending_forward else forward.get(key)
            if forward_sha is None or row['oracle_sha256']==forward_sha:continue
            error=ToolFailure('oracle-state-leak')
            reverse_raw=self.spool.load('reverse',key)
            forward_raw=pending['raw'] if pending_forward else self.spool.load('forward',key)
            prior_histories={'reverse':self.spool.prior('reverse',key)}
            prior_histories['forward']=list(self.spool.histories['forward']) if pending_forward else self.spool.prior('forward',key)
            forward_requests=[request_for(prior) for prior in self.spool.items['forward']]+[request_for(pending['item'])] if pending_forward else self.spool.requests('forward',key)
            evidence={'error':error,'item':item,'oracle':json.loads(reverse_raw),'forward':json.loads(forward_raw),'raw':reverse_raw,'forward_raw':forward_raw,'stderr':self.worker.errors.decode(errors='replace'),'hashes':{'reverse':row,'forward_sha256':forward_sha},'prior_histories':prior_histories,'oracle_history':{'schema':1,'roles':{'oracle':'reverse','candidate':'forward'},'requests':{'oracle':self.spool.requests('reverse',key),'candidate':forward_requests}}}
            yield evidence


def rpc_overlap(forward,reverse):
    """Intersection of two sequential RPC timelines, not summed native CPU time."""
    left=iter(forward);right=iter(reverse);a=next(left,None);b=next(right,None);seconds=0
    while a is not None and b is not None:
        start_a=a['oracle_rpc_started_seconds'];end_a=start_a+a['oracle_rpc_seconds']
        start_b=b['oracle_rpc_started_seconds'];end_b=start_b+b['oracle_rpc_seconds']
        seconds+=max(0,min(end_a,end_b)-max(start_a,start_b))
        if end_a<=end_b:a=next(left,None)
        else:b=next(right,None)
    return seconds


def site_tuple(site):
    if site is None:return None
    if set(site)!= {'file','start','end'} or not isinstance(site['file'],str) or not isinstance(site['start'],int) or not isinstance(site['end'],int) or not 0<=site['start']<site['end']:raise ToolFailure('invalid-source-site')
    return site['file'],site['start'],site['end']


def normalized(answer):
    if answer.get('outcome')!='valid':raise ToolFailure('not-semantic-reply:'+str(answer.get('outcome')))
    if not isinstance(answer.get('declarations'),list) or not isinstance(answer.get('occurrences'),list):raise ToolFailure('incomplete-semantic-reply')
    revisions=answer.get('revisions')
    if not isinstance(revisions,dict) or not revisions or any(not isinstance(name,str) or type(revision) is not int for name,revision in revisions.items()):raise ToolFailure('missing-or-invalid-source-revisions')
    declarations=[];occurrences=[];base_entities=[]
    for field,records in (('declarations',declarations),('base_entities',base_entities)):
        if not isinstance(answer.get(field,[]),list):raise ToolFailure('invalid-semantic-record-list:'+field)
        for d in answer.get(field,[]):
            if 'signatureOwner' not in d or 'bodyOwner' not in d:raise ToolFailure('incomplete-declaration-ownership')
            records.append((site_tuple(d['site']),d['kind'],site_tuple(d['target']),d['name'],site_tuple(d['signatureOwner']),site_tuple(d['bodyOwner'])))
    for o in answer['occurrences']:
        if 'enclosing' not in o:raise ToolFailure('incomplete-occurrence-ownership')
        status=o.get('status','resolved' if o.get('target') else 'unresolved')
        if status not in ('resolved','unresolved','uncertain'):raise ToolFailure('invalid-resolution-status')
        if (status=='resolved')!=(o.get('target') is not None):raise ToolFailure('inconsistent-resolution-status')
        occurrences.append((site_tuple(o['site']),o['role'],site_tuple(o.get('target')),o['binding'],status,site_tuple(o['enclosing'])))
    for records in (declarations,occurrences,base_entities):
        if len(records)!=len(set(records)):raise ToolFailure('duplicate-semantic-record')
    return {'root':answer['root'],'revisions':revisions,'declarations':sorted(declarations,key=repr),'occurrences':sorted(occurrences,key=repr),'base_entities':sorted(base_entities,key=repr)}


def semantic_structure(answer):
    """Alpha/offset-independent graph shape, retaining binder identity and file separation."""
    normalized(answer)
    return _semantic_structure(answer)


def _semantic_structure(answer):
    """Project a reply already validated in this iteration; never revisit its Base."""
    records=answer['declarations']+answer['occurrences']
    sites={site_tuple(record['site']) for record in answer['declarations']}
    sites.update(site_tuple(record['target']) for record in records if record.get('target') and record['target']['file']!='@Base.bend')
    sites.update(site_tuple(record[key]) for record in records for key in ('signatureOwner','bodyOwner','enclosing') if record.get(key) and record[key]['file']!='@Base.bend')
    files={name:index for index,name in enumerate(sorted(answer['revisions']))}
    nodes={site:(files[site[0]],index) for index,site in enumerate(sorted(sites))}
    def node(site):
        if site is None:return None
        if site['file']=='@Base.bend':return ('Base',site['start'],site['end'])
        return nodes[site_tuple(site)]
    declarations=[(record['kind'],node(record['site']),node(record['target']),node(record['signatureOwner']),node(record['bodyOwner'])) for record in sorted(answer['declarations'],key=lambda record:site_tuple(record['site']))]
    occurrences=[(record['role'],record['binding'],node(record.get('target')),record.get('status','resolved'),node(record['enclosing'])) for record in sorted(answer['occurrences'],key=lambda record:(site_tuple(record['site']),record['role']))]
    return {'files':len(files),'declarations':declarations,'occurrences':occurrences}


def compare(expected,actual):
    return _compare_normalized(normalized(expected),normalized(actual))


def _compare_normalized(e,a):
    differences=[]
    if e['root']!=a['root']:differences.append({'class':'root','expected':e['root'],'actual':a['root']})
    if e['revisions']!=a['revisions']:differences.append({'class':'revisions','expected':e['revisions'],'actual':a['revisions']})
    for field in ('declarations','occurrences','base_entities'):
        expected=list(map(repr,e[field]));actual=list(map(repr,a[field]))
        if expected==actual:continue
        expected=Counter(expected);actual=Counter(actual)
        missing=expected-actual;extra=actual-expected
        if missing or extra:differences.append({'class':field,'missing':list(missing.elements()),'extra':list(extra.elements())})
    return differences


def _layout_fingerprint(answer,projection,mapping):
    # Only project-local records change. Reuse this reply's validated, sorted
    # Base tuples, not a Base cached from another compiler call or project.
    local={field:answer[field] for field in ('outcome','root','revisions','declarations','occurrences')}
    remapped=normalized(remap_answer(local,mapping))
    remapped['base_entities']=projection['base_entities']
    return digest(canonical(remapped))


def apply_manifest_requirements(case,corpus):
    """Conjoin global and inline witnesses; never silently replace a requirement."""
    case['assertions']=corpus.get('assertions',{}).get(case['id'],[])+case.get('assertions',[])
    global_error=corpus.get('expected_errors',{}).get(case['id'])
    inline_error=case.get('expected_error')
    if global_error and inline_error and global_error!=inline_error:raise ToolFailure('conflicting-corpus-expected-error:'+case['id'])
    case['expected_error']=inline_error or global_error
    recovery=dict(corpus.get('recovery_assertions',{}).get(case['id'],{}))
    for key,value in case.get('recovery_assertions',{}).items():
        if key in recovery and recovery[key]!=value:raise ToolFailure('conflicting-corpus-recovery-assertion:'+case['id']+':'+key)
        recovery[key]=value
    case['recovery_assertions']=recovery


def validate_source_provenance(case):
    sources=case.get('source_provenance',[])
    if not isinstance(sources,list):raise ToolFailure('invalid-corpus-source-provenance')
    for source in sources:
        if (not isinstance(source,dict) or set(source)!={'repository','commit','path','file','sha256'}
            or any(not isinstance(value,str) for value in source.values())):raise ToolFailure('invalid-corpus-source-provenance')
        path=source['path']
        if (not isinstance(source['repository'],str) or not source['repository'].startswith('https://')
            or not re.fullmatch(r'[0-9a-f]{40}',source['commit'])
            or not isinstance(path,str) or not path.endswith('.bend') or path.startswith('/') or '\\' in path or '..' in path.split('/')
            or source['file'] not in case['files'] or not re.fullmatch(r'[0-9a-f]{64}',source['sha256'])):
            raise ToolFailure('invalid-corpus-source-provenance')
        if digest(case['files'][source['file']].encode())!=source['sha256']:raise ToolFailure('corpus-source-provenance-mismatch')


def make_plan(profile,seed,projects=None):
    corpus=json.loads((SOURCE.parent/'corpus.json').read_text())
    cases=corpus['cases']
    for case in cases:
        apply_manifest_requirements(case,corpus)
        if case.get('overlays'):
            case['disk_files']=dict(case['files']);case['files']={**case['files'],**case['overlays']}
        validate_source_provenance(case)
    if not cases or len(cases)!=corpus['expected_cases'] or sum(len(c['roots']) for c in cases)!=corpus['expected_views']:raise ToolFailure('corpus-count-mismatch')
    if profile=='pr' and projects not in (None,256):raise ToolFailure('required-profile-is-fixed-at-256')
    if profile=='calibration' and projects not in (0,128,256,512):raise ToolFailure('calibration-project-count-required')
    if profile=='corpus' and projects is not None:raise ToolFailure('corpus-has-no-generated-count')
    generated=list(generate(seed,projects if projects is not None else 256)) if profile in ('pr','calibration') else []
    if profile=='pr' and len(generated)!=256:raise ToolFailure('profile-count-mismatch')
    if len({c['id'] for c in cases+generated})!=len(cases+generated):raise ToolFailure('duplicate-case-id')
    planned=[]
    for case in cases+generated:
        validate_bounds(case)
        variants=[('original',case)]
        if case in generated:variants.append(('layout',layout_variant(case)[0]))
        for variant,project in variants:
            for root in project['roots']:
                request_id=case['id']+'/'+root+'/'+variant
                planned.append({'id':request_id,'case_id':case['id'],'root':root,'variant':variant,'facet':case['facet'],'input_sha256':digest(canonical(project)),'case':project,'metamorphic_base':case if variant=='layout' else None})
    result={'schema':1,'profile':profile,'seed':seed,'pin_sha256':digest((SOURCE/'pin.json').read_bytes()),'profile_sha256':digest(canonical(PROFILE)),'planned':planned,'counts':{'projects':len(generated),'corpus_cases':len(cases),'corpus_views':corpus['expected_views'],'generated_views':sum(len(c['roots']) for c in generated),'planned':len(planned)},'corpus_sha256':digest((SOURCE.parent/'corpus.json').read_bytes())}
    result['manifest_sha256']=digest(canonical(result))
    return result


def validate_plan(plan):
    content=dict(plan);recorded=content.pop('manifest_sha256',None)
    if digest(canonical(content))!=recorded:raise ToolFailure('plan-checksum-mismatch')
    regenerated=make_plan(plan['profile'],plan['seed'],plan['counts']['projects'] if plan['profile']=='calibration' else None)
    if canonical(regenerated)!=canonical(plan):raise ToolFailure('stale-plan-or-pin')


def request_for(item):
    case=item['case']
    return {'schema':1,'id':item['id'],'root':item['root'],'files':case['files'],'disk_files':case.get('disk_files',case['files']),'revisions':case['revisions'],'inspect_base':case.get('inspect_base',[])}


def check_manifest_assertions(case,root,answer,recovery=False):
    files=case['files']
    if recovery:
        assertion=case.get('recovery_assertions',{})
        if 'declaration' in assertion:
            name=assertion['declaration'];start=files[root].encode().index(('def '+name).encode())+4
            expected={'file':root,'start':start,'end':start+len(name.encode())}
            if not any(d['site']==expected and d['kind']=='def' and d['name']==name for d in answer['declarations']):raise ToolFailure('recovery-lost-declaration')
        if 'unknown' in assertion:
            name=assertion['unknown'];start=files[root].encode().index((name+'(').encode())
            expected={'file':root,'start':start,'end':start+len(name.encode())}
            if not any(o['site']==expected and o.get('target') is None and o.get('status') in ('unresolved','uncertain') for o in answer['occurrences']):raise ToolFailure('recovery-lost-unknown')
        if 'no_call_at' in assertion:
            start=files[root].encode().index(assertion['no_call_at'].encode())
            if any(o['site']['file']==root and o['site']['start']==start and o['role'] in ('call','offload') for o in answer['occurrences']):raise ToolFailure('recovery-invented-call')
        return
    inspected=case.get('inspect_base',[])
    if sorted(record['name'] for record in answer.get('base_entities',[]))!=sorted(inspected):raise ToolFailure('missing-or-extra-inspected-base-entities')
    for entity in answer.get('base_entities',[]):
        if entity['site']['file']!='@Base.bend' or entity['target']!=entity['site'] or entity['signatureOwner']!=entity['site']:raise ToolFailure('noncanonical-inspected-base-entity')
        if case.get('assert_base_law_body') and (entity['kind']!='law' or entity['bodyOwner'] is None or entity['bodyOwner']==entity['signatureOwner'] or entity['bodyOwner']['file']!='@Base.bend'):raise ToolFailure('base-law-body-owner-not-established')
    for assertion in case.get('assertions',[]):
        file=assertion.get('file',root);positions=[match.start() for match in re.finditer(re.escape(assertion['use'].encode()),files[file].encode())]
        ordinal=assertion.get('occurrence',0)
        if not 0<=ordinal<len(positions):raise ToolFailure('oracle-witness-source-missing')
        start=positions[ordinal]
        matches=[o for o in answer['occurrences'] if o['site']['file']==file and o['site']['start']==start and o['role']==assertion['role']]
        if len(matches)!=1:raise ToolFailure('oracle-witness-occurrence-missing-or-ambiguous')
        occurrence=matches[0];target=occurrence.get('target')
        if occurrence['binding']!=assertion['binding'] or not target or target['file']!=assertion['target_file']:raise ToolFailure('oracle-witness-target-not-established')
        if 'target_kind' in assertion and not any(d['site']==target and d['kind']==assertion['target_kind'] for d in answer['declarations']):raise ToolFailure('oracle-witness-canonical-site-not-established')
        if 'target_use' in assertion:
            anchor=assertion['target_use'].encode();target_file=assertion['target_file']
            positions=[match.start() for match in re.finditer(re.escape(anchor),files[target_file].encode())]
            ordinal=assertion.get('target_occurrence',0)
            if not 0<=ordinal<len(positions):raise ToolFailure('oracle-witness-target-source-missing')
            start=positions[ordinal];expected={'file':target_file,'start':start,'end':start+assertion.get('target_length',len(anchor))}
            if target!=expected:raise ToolFailure('oracle-witness-target-origin-not-established')


def harness_sources():
    sources={path.name:path.read_text() for path in (ROOT/'scripts').glob('semantic_*.py')}
    sources['semantic_gate_profile.json']=(ROOT/'scripts/semantic_gate_profile.json').read_text()
    for name in ('semantic_worker.ts','semantic_pristine_worker.ts','semantic_book_stream.ts','bootstrap_bend.py','bend_compiler_pins.json'):sources[name]=(ROOT/'scripts'/name).read_text()
    return sources


def candidate_source_digest():
    paths=list((ROOT/'src').rglob('*.rs'))+list((ROOT/'tools/semantic-probe/src').rglob('*.rs'))+[ROOT/'tools/semantic-probe/Cargo.toml',ROOT/'Cargo.toml',ROOT/'Cargo.lock',ROOT/'rust-toolchain.toml']
    return digest(canonical({str(path.relative_to(ROOT)):file_digest(path) for path in paths}))


def save_failure(directory,item,oracle,candidate,diff,provenance,command,stderr,raw_replies=None,consumer=None,oracle_history=None):
    harness=harness_sources();binary=Path(command).read_bytes()
    identity={'id':item['id'],'input_sha256':item['input_sha256'],'candidate_sha256':digest(binary),'harness_sha256':digest(canonical(harness)),'pin_sha256':provenance['pin_sha256'],'diff':diff,'consumer_predicate':consumer.get('predicate') if consumer else None}
    if oracle_history is not None:identity['oracle_history']=oracle_history
    directory=Path(directory)/digest(canonical(identity))
    directory.mkdir(parents=True,exist_ok=True)
    if (directory/'original.json').exists():return str(directory)
    payload={'schema':1,'item':item,'request':request_for(item),'oracle':oracle,'candidate':candidate,'diff':diff,'failure_class':','.join(sorted(d['class'] for d in diff)),'provenance':provenance,'candidate_sha256':digest(binary),'candidate_kind':'lsp' if consumer else 'probe','candidate_source_sha256':candidate_source_digest(),'harness':harness,'harness_sha256':digest(canonical(harness)),'stderr':stderr,'compiler_files':{p.name:p.read_text() for p in SOURCE.iterdir() if p.is_file()},'raw_replies':{name:data.decode(errors='replace') for name,data in (raw_replies or {}).items()},'consumer':consumer}
    if oracle_history is not None:
        payload['candidate_kind']='oracle-history';payload['oracle_history']=oracle_history
    payload['raw_replies_base64']={name:base64.b64encode(data).decode('ascii') for name,data in (raw_replies or {}).items()}
    raw=canonical(payload);(directory/'original.json').write_bytes(raw+b'\n')
    compressed=gzip.compress(binary,compresslevel=6,mtime=0);(directory/'candidate.gz').write_bytes(compressed)
    (directory/'sha256.json').write_bytes(canonical({'original.json':digest(raw+b'\n'),'candidate.gz':digest(compressed)})+b'\n')
    return str(directory)


def record_history_failure(report,evidence,artifact_dir,provenance,command,histories):
    item=evidence['item']
    details={'class':'tool-failure','error':str(evidence['error']),'history':'reverse'}
    if 'hashes' in evidence:details['hashes']=evidence['hashes']
    if 'prior_histories' in evidence:
        details['prior_histories']=evidence['prior_histories']
        details['worker_pids']={name:histories[name]['pid'] for name in ('forward','reverse')}
    raw_replies={'oracle':evidence['raw'],'candidate':b''}
    if 'forward_raw' in evidence:
        details['compared_histories']={'oracle':'reverse','candidate':'forward'}
        raw_replies={'reverse_oracle':evidence['raw'],'forward_oracle':evidence['forward_raw']}
    failure=next((failure for failure in report['failures'] if failure.get('history')=='reverse' and failure.get('id')==item['id'] and failure.get('error')==str(evidence['error'])),None)
    if failure is None:
        failure={'class':'tool-failure','error':str(evidence['error']),'id':item['id'],'history':'reverse'}
        report['failures'].append(failure)
    report['status']='fail'
    failure['bundle']=save_failure(artifact_dir,item,evidence['oracle'],evidence.get('forward',{}),[details],provenance,command,{'oracle':evidence['stderr'],'candidate':''},raw_replies,oracle_history=evidence.get('oracle_history'))
    return failure


def execute(plan,candidate,artifact_dir,runtime=None,deadline_seconds=110,lsp=None):
    validate_plan(plan);began=time.monotonic();deadline=began+deadline_seconds
    if runtime is None:runtime=runtime_path()
    installation,provenance=install(runtime,check=True)
    if time.monotonic()>=deadline:raise ToolFailure('deadline-exhausted-during-authentication')
    command=str(Path(candidate).resolve())
    candidate_sha=file_digest(command)
    compiler=probe=None
    counts={'planned':len(plan['planned']),'generated':plan['counts']['projects'],'valid_planned':sum(i['facet']=='valid' for i in plan['planned']),'invalid_planned':sum(i['facet']=='compiler-error' for i in plan['planned']),'recovery_planned':sum(i['facet']=='recovery' for i in plan['planned']),'validated':0,'compared':0,'matched':0,'intended_invalid':0,'recovery':0}
    report={'schema':1,'status':'fail','profile':plan['profile'],'seed':plan['seed'],'manifest_sha256':plan['manifest_sha256'],'pin_sha256':plan['pin_sha256'],'profile_sha256':plan['profile_sha256'],'corpus_sha256':plan['corpus_sha256'],'candidate_sha256':candidate_sha,'planned':[{'id':i['id'],'input_sha256':i['input_sha256']} for i in plan['planned']],'completed':[],'counts':counts,'coverage':{},'failures':[],'provenance':provenance,'metrics':{'startup_seconds':time.monotonic()-began,'case_seconds':[]},'offline':True}
    report['harness_sha256']=digest(canonical(harness_sources()));report['candidate_source_sha256']=candidate_source_digest()
    originals={};oracle_history={};reverse=None;reverse_verified=False;spool=None;forward_pending=None
    original_cases={item['case_id']:item['case'] for item in plan['planned'] if item['variant']=='original'}
    layout_cases={item['case_id'] for item in plan['planned'] if item['variant']!='original'}
    semantic_digests={};structure_hashes={}
    isolation=[i for i in plan['planned'] if i['facet']=='valid']
    histories={'workers':2,'forward':{'planned':len(plan['planned'])}}
    report['metrics']['histories']=histories
    try:
        if len(isolation)<2:raise ToolFailure('insufficient-isolation-cases')
        spool=RawHistorySpool(artifact_dir)
        oracle_answers=SpoolAnswers(spool,'forward')
        compiler,probe=open_workers(native_worker_command(runtime,installation),[command])
        histories['forward']['pid']=compiler.process.pid
        histories['candidate_pid']=probe.process.pid
        reverse=ReverseHistory(native_worker_command(runtime,installation),isolation,provenance,deadline,began,spool)
        histories['reverse']=reverse.metrics
        report['isolation']={'reversed_history':reverse.completed}
        report['metrics']['startup_seconds']=time.monotonic()-began
        histories['forward']['started_seconds']=time.monotonic()-began
        for item in plan['planned']:
            reverse.check()
            start=time.monotonic();request=request_for(item);case_limit=5 if item['case_id'].startswith('generated-') else 15
            oracle={};candidate_reply={};oracle_normalized=candidate_normalized=None;oracle_rpc_started=time.monotonic()
            oracle_pending=compiler.start(request,min(case_limit,deadline-time.monotonic()))
            candidate_rpc_started=time.monotonic();candidate_error=None
            try:candidate_reply=probe.ask(request,min(case_limit,deadline-time.monotonic()))
            except (ToolFailure,OSError,ValueError,KeyError,TypeError,IndexError,UnicodeError) as error:candidate_error=error
            candidate_rpc_seconds=time.monotonic()-candidate_rpc_started
            oracle=compiler.receive(oracle_pending)
            oracle_rpc_seconds=time.monotonic()-oracle_rpc_started
            if oracle.get('provenance')!=provenance:raise ToolFailure('worker-pin-mismatch')
            if oracle['outcome']=='oracle-error':raise ToolFailure('oracle-error:'+oracle.get('error',''))
            # Preserve native evidence before raising a deferred candidate error
            # or a manifest/metamorphic failure cuts off the reverse comparison.
            if oracle['outcome']=='valid':oracle_normalized=normalized(oracle)
            key=item['id'];fingerprint=digest(canonical(oracle_normalized if oracle_normalized is not None else oracle))
            forward_pending={'item':item,'raw':compiler.last_raw,'oracle_sha256':fingerprint} if oracle['outcome']=='valid' and item['facet']=='valid' else None
            spool.append('forward',item,compiler.last_raw,fingerprint)
            if oracle['outcome']=='valid' or item['facet']!='valid':oracle_history[key]=fingerprint
            forward_pending=None
            if oracle['outcome']=='valid' and any(occurrence.get('target') is None for occurrence in oracle.get('occurrences',[])):raise ToolFailure('oracle-origin-ambiguity')
            if candidate_error is not None:raise candidate_error
            diff=[];facet=item['facet'];comparison_started=time.monotonic()
            if facet=='valid':
                if oracle['outcome']!='valid':raise ToolFailure('invalid-generated-input' if item['case_id'].startswith('generated-') else 'unexpected-corpus-rejection')
                check_manifest_assertions(item['case'],item['root'],oracle)
                counts['validated']+=1
                candidate_normalized=normalized(candidate_reply)
                diff=_compare_normalized(oracle_normalized,candidate_normalized);counts['compared']+=1
                if item['variant']=='original':
                    if item['case_id'] in layout_cases:
                        _,mapping=layout_variant(item['case'])
                        originals[(item['case_id'],item['root'])]=(item['id'],_layout_fingerprint(oracle,oracle_normalized,mapping))
                    if item['case_id'].startswith('generated-'):
                        structure_hash=structure_hashes.setdefault(item['case_id'],hashlib.sha256(b'['))
                        if item['root']!=item['case']['roots'][0]:structure_hash.update(b',')
                        structure_hash.update(canonical(_semantic_structure(oracle)))
                        if item['root']==item['case']['roots'][-1]:
                            structure_hash.update(b']');semantic_digests[item['case_id']]=structure_hash.hexdigest()
                            del structure_hashes[item['case_id']]
                else:
                    original_id,layout_fingerprint=originals.pop((item['case_id'],item['root']))
                    if layout_fingerprint!=fingerprint:
                        # Decode the full authenticated original only to explain
                        # a real mismatch, never on every successful layout view.
                        _,mapping=layout_variant(original_cases[item['case_id']])
                        metamorphic=remap_answer(oracle_answers[original_id],mapping)
                        meta_diff=_compare_normalized(normalized(metamorphic),oracle_normalized)
                        raise ToolFailure('compiler-metamorphic-correspondence-failure:'+json.dumps(meta_diff))
                if not diff:counts['matched']+=1
            elif facet in ('compiler-error','recovery'):
                if oracle['outcome']!='compiler-error':raise ToolFailure('intended-invalid-accepted')
                expected_error=item['case'].get('expected_error')
                if expected_error and expected_error not in oracle.get('error',''):raise ToolFailure('wrong-intended-compiler-error')
                expected_rejection=item['case'].get('expected_rejection')
                if expected_rejection and oracle.get('rejection')!=expected_rejection:raise ToolFailure('wrong-intended-rejection-kind')
                if oracle.get('rejection')=='incomplete-input':
                    holes=oracle.get('hole_counts',{})
                    if type(holes.get('instrumented')) is not int or holes['instrumented']<=0 or holes.get('pristine')!=holes['instrumented']:raise ToolFailure('unsubstantiated-incomplete-input-rejection')
                kind=oracle.get('rejection','compiler-error')
                report.setdefault('rejection_counts',{})[kind]=report.get('rejection_counts',{}).get(kind,0)+1
                if candidate_reply['outcome']!='valid':raise ToolFailure('editor-recovery-crash')
                normalized(candidate_reply)
                check_manifest_assertions(item['case'],item['root'],candidate_reply,recovery=True)
                counts['intended_invalid' if facet=='compiler-error' else 'recovery']+=1
            else:raise ToolFailure('unknown-facet')
            comparison_seconds=time.monotonic()-comparison_started
            if diff:
                reverse.cancel()
                bundle=save_failure(artifact_dir,item,oracle,candidate_reply,diff,provenance,command,{'oracle':compiler.errors.decode(errors='replace'),'candidate':probe.errors.decode(errors='replace')},{'oracle':compiler.last_raw,'candidate':probe.last_raw})
                report['failures'].append({'id':key,'class':'semantic-mismatch','diff':diff,'bundle':bundle})
            report['completed'].append({'id':key,'input_sha256':item['input_sha256'],'oracle_sha256':oracle_history[key],'candidate_sha256':digest(canonical(candidate_reply))})
            report['metrics'].setdefault('cases',[]).append({'id':key,'oracle_rpc_started_seconds':oracle_rpc_started-began,'oracle_rpc_seconds':oracle_rpc_seconds,'candidate_rpc_seconds':candidate_rpc_seconds,'comparison_and_manifest_seconds':comparison_seconds,'oracle_stages':oracle.get('metrics',{}),'candidate_stages':candidate_reply.get('metrics',{})})
            if facet=='valid':
                report.setdefault('view_coverage',{})[key]={'planned':1,'validated':1,'compared':1,'matched':int(not diff)}
                expected_records=oracle_normalized['occurrences'];actual_records=Counter(map(repr,candidate_normalized['occurrences']))
                for record in expected_records:
                    role_counts=report.setdefault('role_coverage',{}).setdefault(record[1],{'planned':0,'validated':0,'compared':0,'matched':0})
                    for counter in ('planned','validated','compared'):role_counts[counter]+=1
                    if actual_records[repr(record)]>0:role_counts['matched']+=1;actual_records[repr(record)]-=1
            for family in item['case']['families']:
                family_counts=report['coverage'].setdefault(family,{'planned':0,'validated':0,'compared':0,'matched':0})
                family_counts['planned']+=1;family_counts['validated']+=facet=='valid';family_counts['compared']+=facet=='valid';family_counts['matched']+=facet=='valid' and not diff
            report['metrics']['case_seconds'].append(time.monotonic()-start)
            if diff:
                reverse.close()
                # Persist original before spending any oracle calls on reduction.
                from semantic_replay import save_minimized, shrink
                remaining=max(0,min(30,deadline-time.monotonic()-2))
                if remaining:
                    evidence=json.loads((Path(bundle)/'original.json').read_text())
                    shrink_started=time.monotonic()
                    reduced=shrink(evidence,compiler,probe,time.monotonic()+remaining,64)
                    save_minimized(bundle,reduced)
                    report['failures'][-1]['shrink']={'attempts':reduced['attempts'],'status':reduced['status']}
                    report['metrics']['failure_shrink_seconds']=time.monotonic()-shrink_started
                return report
        histories['forward']['finished_seconds']=time.monotonic()-began
        histories['forward']['completed']=len(report['completed'])
        generated_cases={item['case_id']:item['case'] for item in plan['planned'] if item['case_id'].startswith('generated-') and item['variant']=='original'}
        input_digests={case['input_sha256'] for case in generated_cases.values()}
        report['generator']={'projects':len(generated_cases),'unique_inputs':len(input_digests),'unique_semantic_structures':len(set(semantic_digests.values())),'semantic_digests':semantic_digests,'depth_distribution':dict(Counter(case['depth'] for case in generated_cases.values()))}
        if len(input_digests)!=len(generated_cases) or len(set(semantic_digests.values()))!=len(generated_cases):raise ToolFailure('duplicate-generated-input-or-semantic-structure')
        # The original forward session still proves A/B/A and repetition after
        # every planned forward case, independent of the reverse session.
        proof_contexts=[item for item in isolation if item['case_id']=='proof-context' and item['variant']=='original']
        if len(proof_contexts)!=2:raise ToolFailure('proof-context-isolation-pair-missing')
        first,second=proof_contexts
        isolation_started=time.monotonic()
        for item in (first,second,first,first):
            reverse.check()
            reply=compiler.ask(request_for(item),min(5 if item['case_id'].startswith('generated-') else 15,deadline-time.monotonic()))
            if reply.get('provenance')!=provenance:raise ToolFailure('worker-pin-mismatch')
            if digest(canonical(normalized(reply)))!=oracle_history[item['id']]:raise ToolFailure('oracle-state-leak')
        report['metrics']['forward_aba_repeated_seconds']=time.monotonic()-isolation_started
        reversed_history=reverse.finish(oracle_history)
        reverse_verified=True
        histories['oracle_rpc_overlap_seconds']=rpc_overlap(report['metrics']['cases'],reverse.metrics['cases'])
        # finish joins the reverse owner, including group cleanup, before a
        # fresh native session or the real stdio server increases aggregate RSS.
        fresh_started=time.monotonic()
        histories['fresh_started_seconds']=fresh_started-began
        fresh=Worker(native_worker_command(runtime,installation))
        try:
            histories['fresh_pid']=fresh.process.pid
            fresh_reply=fresh.ask(request_for(first),min(15,deadline-time.monotonic()))
            if fresh_reply.get('provenance')!=provenance:raise ToolFailure('worker-pin-mismatch')
            if digest(canonical(normalized(fresh_reply)))!=oracle_history[first['id']]:raise ToolFailure('fresh-worker-state-leak')
        finally:fresh.close()
        report['metrics']['fresh_worker_seconds']=time.monotonic()-fresh_started
        report['isolation']={'aba':True,'fresh':True,'repeated':True,'aba_views':[first['id'],second['id'],first['id']],'repeated_views':[first['id'],first['id']],'reversed_views':len(reversed_history),'reversed_history':reversed_history}
        from semantic_stdio import run_stdio
        lsp_binary=lsp or str(Path(command).parent/'bend2-lsp')
        stdio_started=time.monotonic()
        histories['stdio_started_seconds']=stdio_started-began
        report['lsp_sha256']=file_digest(lsp_binary)
        report['stdio']=run_stdio(plan,oracle_answers,lsp_binary,deadline)
        report['metrics']['stdio_seconds']=time.monotonic()-stdio_started
        if len(report['completed'])==counts['planned'] and not report['failures']:report['status']='pass'
    except (ToolFailure,OSError,ValueError,KeyError,TypeError,IndexError,UnicodeError) as error:
        if reverse is not None:reverse.cancel()
        failure={'class':'tool-failure','error':str(error),'id':item['id'] if 'item' in locals() else None}
        if hasattr(error,'history_failure'):
            evidence=error.history_failure;failure['id']=evidence['item']['id'];failure['history']='reverse'
        elif hasattr(error,'consumer_item'):
            item=error.consumer_item;failure['id']=item['id'];consumer=error.consumer_evidence
            failure['class']='consumer-mismatch' if consumer.get('predicate') else 'tool-failure'
            wire=b'\n'.join(base64.b64decode(message) for message in consumer['raw_messages_base64'])
            failure['bundle']=save_failure(artifact_dir,item,error.consumer_oracle,consumer['details'],[{'class':failure['class'],'error':str(error)}],provenance,error.consumer_binary,{'oracle':compiler.errors.decode(errors='replace'),'candidate':consumer['stderr']},{'oracle':spool.load('forward',item['id']),'candidate':wire},consumer)
            if consumer.get('predicate') and deadline-time.monotonic()>2:
                from semantic_consumer import shrink_consumer
                from semantic_replay import save_minimized
                evidence=json.loads((Path(failure['bundle'])/'original.json').read_text())
                reduced=shrink_consumer(evidence,compiler,error.consumer_binary,time.monotonic()+min(30,deadline-time.monotonic()-2),64)
                save_minimized(failure['bundle'],reduced)
                failure['shrink']={'attempts':reduced['attempts'],'status':reduced['status']}
        elif 'item' in locals():
            failure['bundle']=save_failure(artifact_dir,item,locals().get('oracle',{}),locals().get('candidate_reply',{}),[{'class':'tool-failure','error':str(error)}],provenance,command,{'oracle':compiler.errors.decode(errors='replace'),'candidate':probe.errors.decode(errors='replace')},{'oracle':compiler.last_raw,'candidate':probe.last_raw})
        report['failures'].append(failure)
    finally:
        # Attempt every owned cleanup even if an earlier cleanup fails. Never
        # replace the original mismatch/timeout with a shutdown exception.
        cleanup_deadline=min(deadline+SHUTDOWN_SECONDS,time.monotonic()+SHUTDOWN_SECONDS)
        retain_spool=False
        for name,owner in (('reverse',reverse),('oracle',compiler),('candidate',probe)):
            if owner is None:continue
            try:owner.close(deadline=cleanup_deadline)
            except BaseException as error:
                report['failures'].append({'class':'cleanup-failure','worker':name,'error':str(error)})
                report['status']='fail'
        if reverse is not None:
            report.setdefault('isolation',{})['reversed_views']=len(reverse.completed)
            if reverse.thread.is_alive():
                retain_spool=True
            else:
                try:
                    if reverse.failure:
                        evidence={**reverse.failure,'stderr':reverse.worker.errors.decode(errors='replace')}
                        record_history_failure(report,evidence,artifact_dir,provenance,command,histories)
                    if spool is not None and not spool.closed and not reverse_verified:
                        for evidence in reverse.divergences(oracle_history,forward_pending):
                            record_history_failure(report,evidence,artifact_dir,provenance,command,histories)
                except BaseException as error:
                    # A failed archive must not turn the only exact original
                    # bytes into a deleted scratch file.
                    retain_spool=True
                    report['failures'].append({'class':'evidence-failure','history':'reverse','error':str(error)})
                    if reverse.failure:
                        report['failures'][-1]['raw_reply_base64']=base64.b64encode(reverse.failure['raw']).decode('ascii')
                    if forward_pending is not None:
                        report['failures'][-1]['forward_raw_reply_base64']=base64.b64encode(forward_pending['raw']).decode('ascii')
                    report['status']='fail'
        if spool is not None and retain_spool:
            report['history_spool']={'path':str(spool.path),'retained':True}
            report['status']='fail'
        if spool is not None and (reverse is None or not reverse.thread.is_alive()):
            try:spool.close(deadline=cleanup_deadline,retain=retain_spool)
            except BaseException as error:
                report['failures'].append({'class':'cleanup-failure','worker':'history-spool','error':str(error)})
                report['status']='fail'
        report['metrics']['elapsed_seconds']=time.monotonic()-began
        rss=resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        report['metrics']['peak_child_rss_bytes']=rss if sys.platform=='darwin' else rss*1024
    return report


def startup_measurement(candidate,runtime=None):
    began=time.monotonic()
    request={'schema':1,'id':'startup','root':'main.bend','files':{'main.bend':'def identity(-A: Type, x: A) -> A:\n  x\n'},'revisions':{'main.bend':1}}
    report={'schema':1,'status':'fail','mode':'startup-only','generated_projects':0,'compared_views':0,'metrics':{},'failures':[],'raw':{'request':request}}
    compiler=None;probe=None
    try:
        if runtime is None:runtime=runtime_path()
        installation,provenance=install(runtime,check=True)
        report['provenance']=provenance
        compiler,probe=open_workers(native_worker_command(runtime,installation),[str(Path(candidate).resolve())])
        spawned=time.monotonic();report['metrics']['prepare_seconds']=spawned-began
        expected=compiler.ask(request,15);report['raw']['oracle']=expected
        actual=probe.ask(request,15);report['raw']['candidate']=actual
        report['metrics']['startup_first_reply_seconds']=time.monotonic()-spawned
        if expected.get('outcome')!='valid':raise ToolFailure('startup-oracle-'+str(expected.get('outcome'))+':'+str(expected.get('error','missing outcome')))
        if actual.get('outcome')!='valid':raise ToolFailure('startup-candidate-'+str(actual.get('outcome'))+':'+str(actual.get('error','missing outcome')))
        differences=compare(expected,actual);report['compared_views']=1
        if differences:
            report['diff']=differences;raise ToolFailure('startup-semantic-mismatch:'+json.dumps(differences))
        report['status']='pass'
    except (ToolFailure,OSError,ValueError,KeyError,TypeError,IndexError,UnicodeError,subprocess.SubprocessError) as error:
        report['failures'].append({'class':'startup-tool-or-semantic-failure','error':str(error)})
    finally:
        for name,worker in (('oracle',compiler),('candidate',probe)):
            if worker is not None:
                worker.close()
                report['raw'][name+'_stdout_base64']=base64.b64encode(worker.last_raw).decode('ascii')
                report['raw'][name+'_stderr']=worker.errors.decode(errors='replace')
        report['metrics']['elapsed_seconds']=time.monotonic()-began
    return report


def main():
    parser=argparse.ArgumentParser(description=__doc__);commands=parser.add_subparsers(dest='command',required=True)
    plan=commands.add_parser('plan');plan.add_argument('--profile',choices=('pr','corpus','calibration'),default='pr');plan.add_argument('--projects',type=int,choices=(0,128,256,512));plan.add_argument('--seed',type=int,default=5301);plan.add_argument('--output',type=Path)
    run=commands.add_parser('run');run.add_argument('--plan',type=Path,required=True);run.add_argument('--candidate',required=True);run.add_argument('--report',type=Path,required=True);run.add_argument('--artifact-dir',type=Path,required=True);run.add_argument('--runtime');run.add_argument('--lsp');run.add_argument('--deadline-seconds',type=float,default=110)
    startup=commands.add_parser('startup');startup.add_argument('--candidate',required=True);startup.add_argument('--report',type=Path,required=True);startup.add_argument('--runtime')
    args=parser.parse_args()
    try:
        if args.command=='startup':
            result=startup_measurement(args.candidate,args.runtime);args.report.parent.mkdir(parents=True,exist_ok=True);args.report.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result));return 0 if result['status']=='pass' else 1
        if args.command=='plan':
            result=make_plan(args.profile,args.seed,args.projects);data=json.dumps(result,indent=2,ensure_ascii=False)+'\n'
            if args.output:args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(data)
            else:print(data,end='')
            return 0
        result=execute(json.loads(args.plan.read_text()),args.candidate,args.artifact_dir,args.runtime,args.deadline_seconds,args.lsp)
        args.report.parent.mkdir(parents=True,exist_ok=True);args.report.write_text(json.dumps(result,indent=2,ensure_ascii=False)+'\n')
        print(json.dumps({'status':result['status'],'counts':result['counts'],'failures':len(result['failures'])}))
        return 0 if result['status']=='pass' else 1
    except (ToolFailure,OSError,ValueError,KeyError,TypeError,IndexError,UnicodeError,subprocess.SubprocessError) as error:
        print('semantic tool failure: '+str(error),file=sys.stderr);return 2


if __name__=='__main__':sys.exit(main())
