#!/usr/bin/env python3
"""Checksummed offline replay and bounded, compiler-valid semantic reduction."""
import argparse
import copy
import gzip
import json
import importlib
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import shutil
import tempfile
import time

from semantic_conformance import ToolFailure, Worker, canonical, compare, open_workers
from semantic_generate import layout_variant, remap_answer, validate_bounds
from semantic_install import ROOT, SOURCE, digest, native_worker_command, runtime_path
from semantic_reduce import structural_reductions


def safe_name(name):
    path=PurePosixPath(name)
    if path.is_absolute() or '..' in path.parts or not path.parts or '\\' in name:raise ToolFailure('unsafe-bundle-path')
    return path


def load_bundle(directory):
    directory=Path(directory)
    if directory.is_symlink():raise ToolFailure('bundle-symlink')
    for path in directory.rglob('*'):
        if path.is_symlink():raise ToolFailure('bundle-symlink')
    if (directory/'sha256.json').stat().st_size>16384:raise ToolFailure('bundle-manifest-limit')
    checksums=json.loads((directory/'sha256.json').read_text())
    if not {'original.json','candidate.gz'}<=checksums.keys():raise ToolFailure('incomplete-bundle')
    for name,expected in checksums.items():
        safe_name(name)
        if (directory/name).stat().st_size>134_217_728:raise ToolFailure('bundle-file-limit')
        if digest((directory/name).read_bytes())!=expected:raise ToolFailure('bundle-checksum-mismatch:'+name)
    bundle=json.loads((directory/'original.json').read_text())
    if bundle.get('schema')!=1:raise ToolFailure('bundle-schema-mismatch')
    if digest(canonical(bundle.get('harness',{})))!=bundle.get('harness_sha256'):raise ToolFailure('bundle-harness-checksum-mismatch')
    for name in bundle['harness']:safe_name(name)
    for name in bundle['request']['files']:safe_name(name)
    if bundle['request']['root'] not in bundle['request']['files']:raise ToolFailure('bundle-missing-root')
    for name,text in bundle['compiler_files'].items():
        safe_name(name)
        expected=bundle['provenance']['pin']['files'].get(name)
        if expected and digest(text.encode())!=expected:raise ToolFailure('bundle-compiler-hash-mismatch')
    return bundle


def predicate(expected,actual,files=None):
    """Conservatively preserve exact local binder/use anchors when scopes shrink."""
    def target_identity(record,answer):
        if record is None:return 'absent'
        site=record.get('target')
        if site is None:return 'unresolved'
        declared=next((d for d in answer['declarations']+answer.get('base_entities',[]) if d['site']==site),None)
        if declared:return canonical([record.get('binding','global'),site['file'],declared['kind'],declared['name']]).decode()
        source=files.get(site['file'],'').encode() if files else b''
        spelling=source[site['start']:site['end']].decode(errors='replace')
        owners=[d for d in answer['declarations'] if d['site']['file']==site['file'] and d['site']['start']<=site['start']]
        owner=max(owners,key=lambda d:d['site']['start'])['name'] if owners else None
        return canonical([record.get('binding'),site,owner,spelling]).decode()
    differences=compare(expected,actual)
    signatures=[]
    if expected['root']!=actual['root']:signatures.append(('root','entry','context',expected['root'],actual['root'],'root'))
    if expected['revisions']!=actual['revisions']:signatures.append(('revisions','source','generation',canonical(expected['revisions']).decode(),canonical(actual['revisions']).decode(),'revisions'))
    for field in ('declarations','occurrences','base_entities'):
        e={canonical([r['site'],r.get('role',r.get('kind'))]):r for r in expected.get(field,[])};a={canonical([r['site'],r.get('role',r.get('kind'))]):r for r in actual.get(field,[])}
        for site in sorted(set(e)|set(a)):
            left=e.get(site);right=a.get(site)
            if left==right:continue
            role=(left or right).get('role',(left or right).get('kind'))
            if left is None:kind='extra'
            elif right is None:kind='missing'
            elif left.get('binding')!=right.get('binding'):kind='binding'
            elif left.get('target')!=right.get('target'):kind='target'
            elif left.get('role')!=right.get('role'):kind='role'
            elif left.get('signatureOwner')!=right.get('signatureOwner'):kind='signature-owner'
            elif left.get('bodyOwner')!=right.get('bodyOwner'):kind='body-owner'
            elif left.get('enclosing')!=right.get('enclosing'):kind='enclosing'
            else:kind='site'
            local=(left and left.get('binding')=='local') or (right and right.get('binding')=='local')
            anchor=canonical((left or right)['site']).decode() if local else 'global-use'
            owners_left=canonical([target_identity({'target':left.get(key),'binding':'global'},expected) for key in ('signatureOwner','bodyOwner','enclosing')]).decode() if left else 'absent'
            owners_right=canonical([target_identity({'target':right.get(key),'binding':'global'},actual) for key in ('signatureOwner','bodyOwner','enclosing')]).decode() if right else 'absent'
            signatures.append((field,role,kind,target_identity(left,expected),target_identity(right,actual),anchor,owners_left,owners_right))
    return tuple(sorted(signatures)) if differences else ()


def reductions(case):
    """Whole grammar productions only; compiler revalidation repairs scope/reachability."""
    # Delete unreachable files and unused imports together, never fabricate a target.
    for name in sorted(case['files']):
        if name in case['roots']:continue
        new=copy.deepcopy(case);del new['files'][name];new['revisions'].pop(name,None)
        for source,text in new['files'].items():
            lines=[]
            for line in text.splitlines(keepends=True):
                m=re.match(r'\s*import\s+(\S+)\s+as\s+',line)
                target=(PurePosixPath(source).parent/m[1]).as_posix() if m else None
                if target and target.removeprefix('./')==name:continue
                lines.append(line)
            new['files'][source]=''.join(lines)
        yield 'remove-module:'+name,new
    if len(case['roots'])>1:
        for root in case['roots']:
            new=copy.deepcopy(case);new['roots']=[r for r in new['roots'] if r!=root]
            yield 'remove-entry:'+root,new
    for name,text in sorted(case['files'].items()):
        # Top-level definitions/types/laws are delimited by the grammar's column-zero heads.
        heads=list(re.finditer(r'(?m)^(?:@unsafe\s+)?(?:def|law|type)\s+[A-Za-z_][A-Za-z0-9_.]*',text))
        for index,head in enumerate(heads):
            end=heads[index+1].start() if index+1<len(heads) else len(text)
            new=copy.deepcopy(case);new['files'][name]=text[:head.start()]+text[end:]
            yield 'remove-declaration:'+name+':'+str(index),new
        stripped=''.join(line for line in text.splitlines(keepends=True) if line.strip() and not line.lstrip().startswith('#'))
        if stripped!=text:
            new=copy.deepcopy(case);new['files'][name]=stripped;yield 'remove-trivia:'+name,new
        # Identifiers renamed as complete dotted semantic atoms, never a prefix substring.
        names=sorted(set(re.findall(r'\b[A-Za-z_][A-Za-z0-9_.]{3,}\b',text)),key=lambda n:(-len(n),n))
        reserved={'import','return','match','case','where','Type','Data','Kind','Quant','Base','def','law','type'}
        for atom in names:
            if atom in reserved or '.' in atom:continue
            short='z'
            if re.search(r'\bz\b',text):continue
            new=copy.deepcopy(case)
            for file,source in new['files'].items():new['files'][file]=re.sub(r'(?<![A-Za-z0-9_.])'+re.escape(atom)+r'(?![A-Za-z0-9_.])',short,source)
            yield 'rename-atom:'+atom,new
        # A complete plain call expression is reduced only to one complete argument.
        for call in re.finditer(r'\b[A-Za-z_][A-Za-z0-9_.]*\(([^()\n,]+)\)',text):
            if text[max(0,call.start()-4):call.start()].endswith('def '):continue
            new=copy.deepcopy(case);new['files'][name]=text[:call.start()]+call[1]+text[call.end():]
            yield 'remove-call-wrapper:'+name,new
    yield from structural_reductions(case)


def shrink(bundle,compiler,probe,deadline,attempt_cap):
    paired=bundle['item'].get('metamorphic_base')
    best=copy.deepcopy(paired or bundle['item']['case']);root=bundle['request']['root']
    signature=predicate(bundle['oracle'],bundle['candidate'],bundle['request']['files'])
    if not signature:raise ToolFailure('bundle-has-no-semantic-mismatch')
    attempts=0;history=[];seen={digest(canonical(best))};stop='local-minimum';failure=None
    def evaluate(project):
        nonlocal attempts,stop
        selected=None
        for entry in project['roots']:
            if attempts>=attempt_cap:stop='attempt-budget';return None
            if time.monotonic()>=deadline:stop='time-budget';return None
            attempts+=1
            request={'schema':1,'id':'shrink-'+str(attempts),'root':entry,'files':project['files'],'revisions':project['revisions']}
            oracle=compiler.ask(request,min(10,deadline-time.monotonic()))
            if oracle['outcome']=='oracle-error':raise ToolFailure('shrink-oracle-failure:'+oracle.get('error',''))
            if oracle['outcome']!='valid':return None
            if entry==root:
                actual=probe.ask(request,min(10,deadline-time.monotonic()))
                if actual['outcome']!='valid':raise ToolFailure('shrink-candidate-failure')
                selected=(oracle,actual)
        return selected
    try:
        pair_signature=None
        if paired:
            baseline=evaluate(best)
            if baseline is None:raise ToolFailure('metamorphic-base-not-valid')
            pair_signature=predicate(*baseline,best['files'])
        while attempts<attempt_cap and time.monotonic()<deadline:
            improved=False
            for label,candidate in reductions(best):
                if root not in candidate['roots']:continue
                checksum=digest(canonical(candidate))
                if checksum in seen:continue
                seen.add(checksum)
                if sum(len(t.encode()) for t in candidate['files'].values())>=sum(len(t.encode()) for t in best['files'].values()) and len(candidate['roots'])>=len(best['roots']):continue
                observed=evaluate(candidate)
                if observed is None:
                    if stop!='local-minimum':break
                    continue
                if paired:
                    if predicate(*observed,candidate['files'])!=pair_signature:continue
                    transformed,mapping=layout_variant(candidate)
                    transformed_reply=evaluate(transformed)
                    if transformed_reply is None:
                        if stop!='local-minimum':break
                        continue
                    if compare(remap_answer(observed[0],mapping),transformed_reply[0]):raise ToolFailure('shrink-compiler-metamorphic-failure')
                    observed=transformed_reply;observed_files=transformed['files']
                else:observed_files=candidate['files']
                if predicate(*observed,observed_files)!=signature:continue
                best=candidate;history.append({'attempt':attempts,'reduction':label,'sha256':checksum});improved=True;break
            if stop!='local-minimum' or not improved:break
    except ToolFailure as error:
        failure=str(error);stop='time-budget' if time.monotonic()>=deadline else 'tool-failure'
    if stop=='local-minimum':
        if attempts>=attempt_cap:stop='attempt-budget'
        elif time.monotonic()>=deadline:stop='time-budget'
    case=layout_variant(best)[0] if paired else best
    return {'schema':1,'case':case,'metamorphic_base':best if paired else None,'predicate':[list(s) for s in signature],'attempts':attempts,'status':stop,'error':failure,'history':history}


def replay(directory,candidate=None,recorded_tools=False,shrink_mode=None,deadline_seconds=None):
    overall_started=time.monotonic()
    budget=600 if shrink_mode=='extended' else 30 if shrink_mode=='pr' else 60
    if deadline_seconds is not None:budget=min(budget,max(0,deadline_seconds))
    overall_deadline=overall_started+budget
    bundle=load_bundle(directory)
    native_history=bundle.get('candidate_kind')=='oracle-history'
    if not native_history and ('reverse_oracle' in bundle.get('raw_replies_base64',{}) or 'oracle-state-leak' in bundle.get('failure_class','') or any('prior_histories' in entry or entry.get('class')=='oracle-state-leak' or 'oracle-state-leak' in entry.get('error','') for entry in bundle.get('diff',[]))):
        raise ToolFailure('hash-only-native-history-not-replayable')
    harness_matches=all((ROOT/'scripts'/name).is_file() and (ROOT/'scripts'/name).read_text()==text for name,text in bundle['harness'].items())
    if not harness_matches and not recorded_tools:raise ToolFailure('harness-pin-mismatch; explicitly select --recorded-tools')
    selected_runtime=runtime_path(runtime_pin=json.loads(bundle['compiler_files']['runtime-pin.json'])) if recorded_tools else runtime_path()
    # Reconstruct exclusively from recorded source; no original temp paths or network.
    with tempfile.TemporaryDirectory(prefix='bend-semantic-replay-') as temporary:
        directory_root=Path(temporary);scripts=directory_root/'scripts';sources=directory_root/'tests/fixtures/semantic/compiler'
        scripts.mkdir();sources.mkdir(parents=True)
        for name,text in bundle['harness'].items():safe_name(name);(scripts/name).write_text(text)
        for name,text in bundle['compiler_files'].items():safe_name(name);(sources/name).write_text(text)
        runtime_archive=Path(selected_runtime).parent/'runtime.zip'
        if not runtime_archive.is_file():raise ToolFailure('authenticated-runtime-archive-missing')
        runtime_cache=directory_root/'target/semantic-tools/runtime'/runtime_archive.parent.name
        runtime_cache.mkdir(parents=True)
        shutil.copyfile(runtime_archive,runtime_cache/'runtime.zip')
        shutil.copyfile(selected_runtime,runtime_cache/'bun');(runtime_cache/'bun').chmod(0o755)
        if not native_history:
            if candidate is None:
                compressed=Path(directory)/'candidate.gz'
                if not compressed.is_file():raise ToolFailure('missing-recorded-candidate; supply --candidate with exact recorded checksum')
                binary=directory_root/'semantic-probe'
                with gzip.open(compressed,'rb') as stream:content=stream.read(268_435_457)
                if len(content)>268_435_456:raise ToolFailure('candidate-decompression-limit')
                binary.write_bytes(content);binary.chmod(0o755);candidate=str(binary)
            if digest(Path(candidate).read_bytes())!=bundle['candidate_sha256']:raise ToolFailure('candidate-pin-mismatch')
        if not harness_matches:
            previous={name:module for name,module in sys.modules.items() if name.startswith('semantic_') or name=='bootstrap_bend'}
            for name in previous:sys.modules.pop(name,None)
            sys.path.insert(0,str(scripts))
            try:
                recorded=importlib.import_module('semantic_replay')
                return recorded.replay(directory,candidate,False,shrink_mode,max(0,overall_deadline-time.monotonic()))
            except Exception as error:raise ToolFailure('recorded-harness-failure:'+str(error)) from error
            finally:
                sys.path.pop(0)
                for name in list(sys.modules):
                    if name.startswith('semantic_') or name=='bootstrap_bend':sys.modules.pop(name,None)
                sys.modules.update(previous)
        # Load recorded installer in a distinct Python process, not current instrumenter.
        installed=subprocess.run([sys.executable,str(scripts/'semantic_install.py'),'--runtime',selected_runtime],capture_output=True,text=True,timeout=max(.01,min(20,overall_deadline-time.monotonic())),check=True)
        installation=Path(installed.stdout.strip())
        provenance=json.loads((installation/'provenance.json').read_text())
        for field in ('pin_sha256','adapter_sha256','worker_sha256','pristine_worker_sha256','book_stream_sha256','runtime_sha256'):
            if provenance[field]!=bundle['provenance'][field]:raise ToolFailure('recorded-tool-pin-mismatch:'+field)
        if native_history:
            from semantic_history_replay import replay_history
            result=replay_history(bundle,native_worker_command(selected_runtime,installation),overall_deadline,shrink_mode)
            if shrink_mode:
                save_minimized(directory,result['shrink'])
                if result['shrink']['status']=='tool-failure':raise ToolFailure('native-history-shrink-tool-failure:'+str(result['shrink']['error']))
            result['elapsed_seconds']=time.monotonic()-overall_started
            return result
        if bundle.get('candidate_kind')=='lsp':
            from semantic_consumer import replay_consumer, shrink_consumer
            compiler=Worker(native_worker_command(selected_runtime,installation))
            try:
                result=replay_consumer(bundle,compiler,str(Path(candidate).resolve()),overall_deadline)
                if shrink_mode:
                    cap,seconds=(64,30) if shrink_mode=='pr' else (2000,600)
                    reduced=shrink_consumer(bundle,compiler,str(Path(candidate).resolve()),min(overall_deadline-2,time.monotonic()+seconds),cap)
                    result['shrink']=reduced;save_minimized(directory,reduced)
                    if reduced['status']=='tool-failure':raise ToolFailure('consumer-shrink-tool-failure:'+str(reduced['error']))
                result['elapsed_seconds']=time.monotonic()-overall_started
                return result
            finally:compiler.close()
        compiler,probe=open_workers(native_worker_command(selected_runtime,installation),[str(Path(candidate).resolve())])
        began=overall_started
        try:
            request=bundle['request'];first=compiler.ask(request,min(15,overall_deadline-time.monotonic()));actual=probe.ask(request,min(10,overall_deadline-time.monotonic()))
            signature=predicate(bundle['oracle'],bundle['candidate'],request['files'])
            if first['outcome']!='valid' or actual['outcome']!='valid' or predicate(first,actual,request['files'])!=signature:raise ToolFailure('mismatch-not-reproduced')
            repeated=compiler.ask(request,min(15,overall_deadline-time.monotonic()));again=probe.ask(request,min(10,overall_deadline-time.monotonic()))
            if compare(first,repeated) or compare(actual,again) or first.get('provenance')!=repeated.get('provenance'):raise ToolFailure('nondeterministic-replay')
            result={'schema':1,'status':'reproduced','predicate':[list(s) for s in signature],'elapsed_seconds':time.monotonic()-began}
            if shrink_mode:
                cap,seconds=(64,30) if shrink_mode=='pr' else (2000,600)
                seconds=min(seconds,max(0,overall_deadline-time.monotonic()-2))
                reduced=shrink(bundle,compiler,probe,time.monotonic()+seconds,cap)
                result['shrink']=reduced
                save_minimized(directory,reduced)
                if reduced['status']=='tool-failure':raise ToolFailure('shrink-tool-failure:'+str(reduced['error']))
            return result
        finally:compiler.close();probe.close()


def save_minimized(directory,reduced):
    raw=canonical(reduced)+b'\n';directory=Path(directory)
    (directory/'minimized.json').write_bytes(raw)
    (directory/'minimized.sha256').write_text(digest(raw)+'\n')


def promote_bundle(directory,output):
    bundle=load_bundle(directory)
    if bundle.get('candidate_kind')=='oracle-history' or 'reverse_oracle' in bundle.get('raw_replies_base64',{}) or 'oracle-state-leak' in bundle.get('failure_class','') or any('prior_histories' in entry or 'oracle-state-leak' in entry.get('error','') for entry in bundle.get('diff',[])):
        raise ToolFailure('oracle-integrity-evidence-is-not-promotable')
    raw=(directory/'minimized.json').read_bytes()
    if digest(raw)!=(directory/'minimized.sha256').read_text().strip():raise ToolFailure('minimized-checksum-mismatch')
    reduced=json.loads(raw);case=reduced['case']
    validate_bounds(case)
    for name in case['files']:safe_name(name)
    if bundle['provenance']['pin_sha256']!=digest((SOURCE/'pin.json').read_bytes()):raise ToolFailure('promotion-requires-recorded-compiler-pin')
    case['id']='regression-'+digest(canonical(case))[:16]
    case['facet']='valid';case['oracle_provenance']=bundle['provenance']
    case['expected_relation']={'compiler_valid':True,'no_missing_or_extra_edges':True,'original_mismatch_predicate':reduced['predicate']}
    case['rationale']='Compiler-valid minimized counterexample; compare every declaration, source site, role and target against the recorded pinned compiler, not failed candidate output.'
    if output.exists():
        corpus=json.loads(output.read_text())
        if corpus.get('schema')!=1 or not isinstance(corpus.get('cases'),list):raise ToolFailure('promotion-output-is-not-corpus')
        if any(item['id']==case['id'] for item in corpus['cases']):raise ToolFailure('duplicate-promoted-case')
        if corpus['pin']!=bundle['provenance']['pin']['id']:raise ToolFailure('promotion-corpus-pin-mismatch')
        corpus['cases'].append(case);corpus['expected_cases']=len(corpus['cases']);corpus['expected_views']=sum(len(item['roots']) for item in corpus['cases']);corpus['version']+=1
        output.write_text(json.dumps(corpus,indent=2,ensure_ascii=False)+'\n')
    else:output.parent.mkdir(parents=True,exist_ok=True);output.write_text(json.dumps(case,indent=2,ensure_ascii=False)+'\n')


def main():
    parser=argparse.ArgumentParser(description=__doc__);commands=parser.add_subparsers(dest='command',required=True)
    run=commands.add_parser('replay');run.add_argument('bundle',type=Path);run.add_argument('--candidate');run.add_argument('--recorded-tools',action='store_true');run.add_argument('--shrink',choices=('pr','extended'));run.add_argument('--deadline-seconds',type=float)
    promote=commands.add_parser('promote');promote.add_argument('bundle',type=Path);promote.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    try:
        if args.command=='promote':
            promote_bundle(args.bundle,args.output);return 0
        print(json.dumps(replay(args.bundle,args.candidate,args.recorded_tools,args.shrink,args.deadline_seconds),indent=2));return 0
    except (ToolFailure,ValueError,OSError,KeyError,TypeError,IndexError,UnicodeError,subprocess.SubprocessError) as error:print('semantic replay failure: '+str(error),file=sys.stderr);return 2


if __name__=='__main__':sys.exit(main())
