#!/usr/bin/env python3
"""Real compiler differential conformance. Tool/protocol errors never mean agreement."""
import argparse
from collections import Counter
import hashlib
import gzip
import base64
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
from semantic_install import ROOT, SOURCE, digest, install, runtime_path
from semantic_process import signal_group

SCHEMA=1
MAX_OUTPUT=4_000_000
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
                        try:self.lines.put_nowait(bytes(line))
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

    def ask(self,request,timeout):
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
            self._fail('worker-crash');raise ToolFailure('worker-crash') from error
        try:line=self.lines.get(timeout=max(0,deadline-time.monotonic()))
        except queue.Empty as error:
            self._fail('worker-timeout');raise ToolFailure('worker-timeout') from error
        with self.lock:
            if self.failure:raise self.failure
        if isinstance(line,Exception):raise line
        self.last_raw=line
        try:reply=json.loads(line,parse_constant=lambda value:float('invalid-'+value))
        except (ValueError,UnicodeError) as error:self._fail('malformed-reply');raise ToolFailure('malformed-reply') from error
        if not isinstance(reply,dict) or reply.get('schema')!=SCHEMA or reply.get('id')!=request['id'] or reply.get('root')!=request['root']:
            self._fail('stale-or-incomplete-reply');raise ToolFailure('stale-or-incomplete-reply')
        if reply.get('outcome') not in ('valid','compiler-error','oracle-error'):
            self._fail('missing-outcome');raise ToolFailure('missing-outcome')
        return reply

    def close(self):
        with self.lock:self.closed=True
        # The session leader may already have exited while descendants hold pipes.
        signal_group(self.process,signal.SIGTERM)
        try:self.process.wait(timeout=1)
        except subprocess.TimeoutExpired:pass
        # Always escalate the group, even if waiting for the leader succeeded.
        signal_group(self.process,signal.SIGKILL)
        self.process.wait(timeout=2)
        self.reader.join(timeout=1);self.err_reader.join(timeout=1)
        for stream in (self.process.stdin,self.process.stdout,self.process.stderr):stream.close()
        self.home.cleanup()




def open_workers(compiler_command,candidate_command):
    compiler=Worker(compiler_command)
    try:probe=Worker(candidate_command)
    except BaseException:compiler.close();raise
    return compiler,probe


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
    e=normalized(expected);a=normalized(actual);differences=[]
    if e['root']!=a['root']:differences.append({'class':'root','expected':e['root'],'actual':a['root']})
    if e['revisions']!=a['revisions']:differences.append({'class':'revisions','expected':e['revisions'],'actual':a['revisions']})
    for field in ('declarations','occurrences','base_entities'):
        missing=Counter(map(repr,e[field]))-Counter(map(repr,a[field]));extra=Counter(map(repr,a[field]))-Counter(map(repr,e[field]))
        if missing or extra:differences.append({'class':field,'missing':list(missing.elements()),'extra':list(extra.elements())})
    return differences


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
    for name in ('semantic_worker.ts','semantic_pristine_worker.ts','semantic_book_stream.ts','bootstrap_bend.py','bend_compiler_pins.json'):sources[name]=(ROOT/'scripts'/name).read_text()
    return sources


def candidate_source_digest():
    paths=list((ROOT/'src').rglob('*.rs'))+[ROOT/'examples/semantic_probe.rs',ROOT/'Cargo.toml',ROOT/'Cargo.lock',ROOT/'rust-toolchain.toml']
    return digest(canonical({str(path.relative_to(ROOT)):digest(path.read_bytes()) for path in paths}))


def save_failure(directory,item,oracle,candidate,diff,provenance,command,stderr,raw_replies=None,consumer=None):
    harness=harness_sources();binary=Path(command).read_bytes()
    identity={'id':item['id'],'input_sha256':item['input_sha256'],'candidate_sha256':digest(binary),'harness_sha256':digest(canonical(harness)),'pin_sha256':provenance['pin_sha256'],'diff':diff,'consumer_predicate':consumer.get('predicate') if consumer else None}
    directory=Path(directory)/digest(canonical(identity))
    directory.mkdir(parents=True,exist_ok=True)
    if (directory/'original.json').exists():return str(directory)
    payload={'schema':1,'item':item,'request':request_for(item),'oracle':oracle,'candidate':candidate,'diff':diff,'failure_class':','.join(sorted(d['class'] for d in diff)),'provenance':provenance,'candidate_sha256':digest(binary),'candidate_kind':'lsp' if consumer else 'probe','candidate_source_sha256':candidate_source_digest(),'harness':harness,'harness_sha256':digest(canonical(harness)),'stderr':stderr,'compiler_files':{p.name:p.read_text() for p in SOURCE.iterdir() if p.is_file()},'raw_replies':{name:data.decode(errors='replace') for name,data in (raw_replies or {}).items()},'consumer':consumer}
    payload['raw_replies_base64']={name:base64.b64encode(data).decode('ascii') for name,data in (raw_replies or {}).items()}
    raw=canonical(payload);(directory/'original.json').write_bytes(raw+b'\n')
    compressed=gzip.compress(binary,compresslevel=6,mtime=0);(directory/'candidate.gz').write_bytes(compressed)
    (directory/'sha256.json').write_bytes(canonical({'original.json':digest(raw+b'\n'),'candidate.gz':digest(compressed)})+b'\n')
    return str(directory)


def execute(plan,candidate,artifact_dir,runtime=None,deadline_seconds=110,lsp=None):
    validate_plan(plan);began=time.monotonic();deadline=began+deadline_seconds
    if runtime is None:runtime=runtime_path()
    installation,provenance=install(runtime,check=True)
    if time.monotonic()>=deadline:raise ToolFailure('deadline-exhausted-during-authentication')
    command=str(Path(candidate).resolve())
    candidate_sha=digest(Path(command).read_bytes())
    compiler,probe=open_workers([runtime,str(installation/'worker.ts')],[command])
    counts={'planned':len(plan['planned']),'generated':plan['counts']['projects'],'valid_planned':sum(i['facet']=='valid' for i in plan['planned']),'invalid_planned':sum(i['facet']=='compiler-error' for i in plan['planned']),'recovery_planned':sum(i['facet']=='recovery' for i in plan['planned']),'validated':0,'compared':0,'matched':0,'intended_invalid':0,'recovery':0}
    report={'schema':1,'status':'fail','profile':plan['profile'],'seed':plan['seed'],'manifest_sha256':plan['manifest_sha256'],'pin_sha256':plan['pin_sha256'],'profile_sha256':plan['profile_sha256'],'corpus_sha256':plan['corpus_sha256'],'candidate_sha256':candidate_sha,'planned':[{'id':i['id'],'input_sha256':i['input_sha256']} for i in plan['planned']],'completed':[],'counts':counts,'coverage':{},'failures':[],'provenance':provenance,'metrics':{'startup_seconds':time.monotonic()-began,'case_seconds':[]},'offline':True}
    report['harness_sha256']=digest(canonical(harness_sources()));report['candidate_source_sha256']=candidate_source_digest()
    originals={};oracle_history={};oracle_answers={};stdio_oracle_raw={}
    from semantic_stdio import SELECTED
    try:
        for item in plan['planned']:
            start=time.monotonic();request=request_for(item);case_limit=5 if item['case_id'].startswith('generated-') else 15
            oracle={};candidate_reply={};oracle_rpc_started=time.monotonic()
            oracle=compiler.ask(request,min(case_limit,deadline-time.monotonic()))
            oracle_rpc_seconds=time.monotonic()-oracle_rpc_started
            oracle_answers[item['id']]=oracle
            if (item['case_id'] in SELECTED or item['case'].get('consumer_profile')) and item['variant']=='original':stdio_oracle_raw[item['id']]=compiler.last_raw
            if oracle.get('provenance')!=provenance:raise ToolFailure('worker-pin-mismatch')
            if oracle['outcome']=='oracle-error':raise ToolFailure('oracle-error:'+oracle.get('error',''))
            if oracle['outcome']=='valid' and any(occurrence.get('target') is None for occurrence in oracle.get('occurrences',[])):raise ToolFailure('oracle-origin-ambiguity')
            candidate_rpc_started=time.monotonic()
            candidate_reply=probe.ask(request,min(case_limit,deadline-time.monotonic()))
            candidate_rpc_seconds=time.monotonic()-candidate_rpc_started
            diff=[];facet=item['facet'];comparison_started=time.monotonic()
            if facet=='valid':
                if oracle['outcome']!='valid':raise ToolFailure('invalid-generated-input' if item['case_id'].startswith('generated-') else 'unexpected-corpus-rejection')
                check_manifest_assertions(item['case'],item['root'],oracle)
                counts['validated']+=1
                diff=compare(oracle,candidate_reply);counts['compared']+=1
                if item['variant']=='original':originals[(item['case_id'],item['root'])]=oracle
                else:
                    original_case=next(p['case'] for p in plan['planned'] if p['case_id']==item['case_id'] and p['variant']=='original')
                    _,mapping=layout_variant(original_case)
                    metamorphic=remap_answer(originals[(item['case_id'],item['root'])],mapping)
                    meta_diff=compare(metamorphic,oracle)
                    if meta_diff:raise ToolFailure('compiler-metamorphic-correspondence-failure:'+json.dumps(meta_diff))
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
            key=item['id'];oracle_history[key]=digest(canonical(normalized(oracle))) if facet=='valid' else digest(canonical(oracle))
            comparison_seconds=time.monotonic()-comparison_started
            if diff:
                bundle=save_failure(artifact_dir,item,oracle,candidate_reply,diff,provenance,command,{'oracle':compiler.errors.decode(errors='replace'),'candidate':probe.errors.decode(errors='replace')},{'oracle':compiler.last_raw,'candidate':probe.last_raw})
                report['failures'].append({'id':key,'class':'semantic-mismatch','diff':diff,'bundle':bundle})
            report['completed'].append({'id':key,'input_sha256':item['input_sha256'],'oracle_sha256':oracle_history[key],'candidate_sha256':digest(canonical(candidate_reply))})
            report['metrics'].setdefault('cases',[]).append({'id':key,'oracle_rpc_seconds':oracle_rpc_seconds,'candidate_rpc_seconds':candidate_rpc_seconds,'comparison_and_manifest_seconds':comparison_seconds,'oracle_stages':oracle.get('metrics',{}),'candidate_stages':candidate_reply.get('metrics',{})})
            if facet=='valid':
                report.setdefault('view_coverage',{})[key]={'planned':1,'validated':1,'compared':1,'matched':int(not diff)}
                expected_records=normalized(oracle)['occurrences'];actual_records=Counter(map(repr,normalized(candidate_reply)['occurrences']))
                for record in expected_records:
                    role_counts=report.setdefault('role_coverage',{}).setdefault(record[1],{'planned':0,'validated':0,'compared':0,'matched':0})
                    for counter in ('planned','validated','compared'):role_counts[counter]+=1
                    if actual_records[repr(record)]>0:role_counts['matched']+=1;actual_records[repr(record)]-=1
            for family in item['case']['families']:
                family_counts=report['coverage'].setdefault(family,{'planned':0,'validated':0,'compared':0,'matched':0})
                family_counts['planned']+=1;family_counts['validated']+=facet=='valid';family_counts['compared']+=facet=='valid';family_counts['matched']+=facet=='valid' and not diff
            report['metrics']['case_seconds'].append(time.monotonic()-start)
            if diff:
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
        generated_cases={item['case_id']:item['case'] for item in plan['planned'] if item['case_id'].startswith('generated-') and item['variant']=='original'}
        semantic_digests={case_id:digest(canonical([semantic_structure(oracle_answers[case_id+'/'+root+'/original']) for root in case['roots']])) for case_id,case in generated_cases.items()}
        input_digests={case['input_sha256'] for case in generated_cases.values()}
        report['generator']={'projects':len(generated_cases),'unique_inputs':len(input_digests),'unique_semantic_structures':len(set(semantic_digests.values())),'semantic_digests':semantic_digests,'depth_distribution':dict(Counter(case['depth'] for case in generated_cases.values()))}
        if len(input_digests)!=len(generated_cases) or len(set(semantic_digests.values()))!=len(generated_cases):raise ToolFailure('duplicate-generated-input-or-semantic-structure')
        # Required fresh-Book A/B/A, repeated-case, and reversed order checks.
        isolation=[i for i in plan['planned'] if i['facet']=='valid']
        if len(isolation)<2:raise ToolFailure('insufficient-isolation-cases')
        proof_contexts=[item for item in isolation if item['case_id']=='proof-context' and item['variant']=='original']
        if len(proof_contexts)!=2:raise ToolFailure('proof-context-isolation-pair-missing')
        first,second=proof_contexts
        sequence=[first,second,first,first]+list(reversed(isolation))
        for item in sequence:
            reply=compiler.ask(request_for(item),min(5 if item['case_id'].startswith('generated-') else 15,deadline-time.monotonic()))
            if digest(canonical(normalized(reply)))!=oracle_history[item['id']]:raise ToolFailure('oracle-state-leak')
        fresh=Worker([runtime,str(installation/'worker.ts')])
        try:
            fresh_reply=fresh.ask(request_for(first),min(15,deadline-time.monotonic()))
            if digest(canonical(normalized(fresh_reply)))!=oracle_history[first['id']]:raise ToolFailure('fresh-worker-state-leak')
        finally:fresh.close()
        report['isolation']={'aba':True,'fresh':True,'repeated':True,'aba_views':[first['id'],second['id'],first['id']],'reversed_views':len(isolation)}
        from semantic_stdio import run_stdio
        lsp_binary=lsp or str(Path(command).parent.parent/'bend2-lsp')
        stdio_started=time.monotonic()
        report['lsp_sha256']=digest(Path(lsp_binary).read_bytes())
        report['stdio']=run_stdio(plan,oracle_answers,lsp_binary,deadline)
        report['metrics']['stdio_seconds']=time.monotonic()-stdio_started
        if len(report['completed'])==counts['planned'] and not report['failures']:report['status']='pass'
    except (ToolFailure,OSError,ValueError,KeyError,TypeError,IndexError,UnicodeError) as error:
        failure={'class':'tool-failure','error':str(error),'id':item['id'] if 'item' in locals() else None}
        if hasattr(error,'consumer_item'):
            item=error.consumer_item;failure['id']=item['id'];consumer=error.consumer_evidence
            failure['class']='consumer-mismatch' if consumer.get('predicate') else 'tool-failure'
            wire=b'\n'.join(base64.b64decode(message) for message in consumer['raw_messages_base64'])
            failure['bundle']=save_failure(artifact_dir,item,error.consumer_oracle,consumer['details'],[{'class':failure['class'],'error':str(error)}],provenance,error.consumer_binary,{'oracle':compiler.errors.decode(errors='replace'),'candidate':consumer['stderr']},{'oracle':stdio_oracle_raw.get(item['id'],b''),'candidate':wire},consumer)
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
        compiler.close();probe.close()
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
        compiler,probe=open_workers([runtime,str(installation/'worker.ts')],[str(Path(candidate).resolve())])
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
