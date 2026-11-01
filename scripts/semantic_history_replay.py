"""Replay oracle-integrity evidence in independently owned fresh native sessions."""
import base64
import copy
import json
import threading
import time

from semantic_conformance import MAX_OUTPUT, SHUTDOWN_SECONDS, ToolFailure, Worker, canonical, normalized, request_for
from semantic_generate import validate_bounds
from semantic_install import digest


def validate_history(bundle):
    from semantic_replay import predicate, safe_name
    history=bundle.get('oracle_history')
    if not isinstance(history,dict) or set(history)!={'schema','roles','requests'} or history['schema']!=1 or history['roles']!={'oracle':'reverse','candidate':'forward'}:
        raise ToolFailure('missing-or-invalid-native-history; hash-only histories cannot be replayed')
    requests=history['requests']
    if not isinstance(requests,dict) or set(requests)!={'oracle','candidate'}:raise ToolFailure('invalid-native-history-requests')
    if bundle['request']!=request_for(bundle['item']) or digest(canonical(bundle['item']['case']))!=bundle['item']['input_sha256']:
        raise ToolFailure('native-history-target-input-mismatch')
    for role,sequence in requests.items():
        if not isinstance(sequence,list) or not sequence or sequence[-1]!=bundle['request']:raise ToolFailure('native-history-target-not-last:'+role)
        for request in sequence:
            if not isinstance(request,dict) or request.get('schema')!=1 or not isinstance(request.get('id'),str) or not request['id'] or len(canonical(request))>300000:
                raise ToolFailure('invalid-native-history-request')
            files=request['files'];revisions=request['revisions']
            if not isinstance(files,dict) or any(not isinstance(text,str) for text in files.values()):raise ToolFailure('invalid-native-history-files')
            for name in files:safe_name(name)
            if request['root'] not in files or set(revisions)!=set(files) or any(type(value) is not int for value in revisions.values()):raise ToolFailure('invalid-native-history-revisions')
            validate_bounds({'files':files,'roots':[request['root']]})
            disk=request.get('disk_files',files)
            for name,text in disk.items():
                safe_name(name)
                if not isinstance(text,str):raise ToolFailure('invalid-native-history-disk-files')
            validate_bounds({'files':disk,'roots':[request['root']]})
    for role,raw_name in (('oracle','reverse_oracle'),('candidate','forward_oracle')):
        encoded=bundle.get('raw_replies_base64',{}).get(raw_name,bundle.get('raw_replies_base64',{}).get(role))
        if not isinstance(encoded,str):raise ToolFailure('missing-native-history-raw:'+role)
        try:raw=base64.b64decode(encoded,validate=True);answer=json.loads(raw)
        except (ValueError,UnicodeError) as error:raise ToolFailure('invalid-native-history-raw:'+role) from error
        if len(raw)>MAX_OUTPUT or answer!=bundle[role]:raise ToolFailure('native-history-raw-mismatch:'+role)
        if answer.get('schema')!=1 or answer.get('id')!=bundle['request']['id'] or answer.get('root')!=bundle['request']['root'] or answer.get('provenance')!=bundle['provenance']:raise ToolFailure('native-history-recorded-pin-mismatch')
        normalized(answer)
    for detail in bundle.get('diff',[]):
        hashes=detail.get('hashes',{})
        if hashes:
            if hashes.get('reverse',{}).get('oracle_sha256')!=digest(canonical(normalized(bundle['oracle']))) or hashes.get('forward_sha256')!=digest(canonical(normalized(bundle['candidate']))):
                raise ToolFailure('native-history-recorded-semantic-hash-mismatch')
        for role,label in (('oracle','reverse'),('candidate','forward')):
            prior=detail.get('prior_histories',{}).get(label)
            if prior is not None and [row['id'] for row in prior]!=[request['id'] for request in requests[role][:-1]]:
                raise ToolFailure('native-history-prior-order-mismatch')
    signature=predicate(bundle['oracle'],bundle['candidate'],bundle['request']['files'])
    if not signature:raise ToolFailure('native-history-has-no-divergence')
    return requests,signature


def run_histories(requests,command,provenance,deadline):
    """Run both histories concurrently, with one global operation/cleanup cutoff."""
    results={};errors=[];workers=[];threads=[]
    def consume(role,worker):
        try:
            for request in requests[role]:
                answer=worker.ask(request,min(15,deadline-time.monotonic()))
                if answer.get('provenance')!=provenance:raise ToolFailure('native-history-worker-pin-mismatch')
                if answer['outcome']=='oracle-error':raise ToolFailure('native-history-oracle-error:'+answer.get('error',''))
                if answer['outcome']=='valid':normalized(answer)
            if answer['outcome']!='valid':raise ToolFailure('native-history-target-not-valid')
            results[role]=answer
        except BaseException as error:errors.append(error)
    try:
        for role in ('oracle','candidate'):
            if time.monotonic()>=deadline:raise ToolFailure('native-history-deadline')
            worker=Worker(command);workers.append(worker)
            thread=threading.Thread(target=consume,args=(role,worker),daemon=True);threads.append(thread);thread.start()
        for thread in threads:
            thread.join(max(0,deadline-time.monotonic()))
        if any(thread.is_alive() for thread in threads):raise ToolFailure('native-history-deadline')
        if errors:raise errors[0]
        return results
    finally:
        cleanup=time.monotonic()+SHUTDOWN_SECONDS;cleanup_errors=[]
        for worker,thread in zip(workers,threads):
            if thread.is_alive():worker._fail('native-history-cancelled')
        for worker in workers:
            try:worker.close(cleanup)
            except BaseException as error:cleanup_errors.append(error)
        for thread in threads:thread.join(max(0,cleanup-time.monotonic()))
        if any(thread.is_alive() for thread in threads):cleanup_errors.append(ToolFailure('native-history-thread-shutdown-timeout'))
        if cleanup_errors:raise cleanup_errors[0]


def reproduces(bundle,observed,signature,exact=False):
    from semantic_replay import predicate
    if predicate(observed['oracle'],observed['candidate'],bundle['request']['files'])!=signature:return False
    return not exact or all(normalized(observed[role])==normalized(bundle[role]) for role in ('oracle','candidate'))


def shrink_history(bundle,command,deadline,attempt_cap):
    requests,signature=validate_history(bundle);best=copy.deepcopy(requests)
    attempts=0;changes=[];status='local-minimum';failure=None
    try:
        improved=True
        while improved:
            improved=False
            for role in ('oracle','candidate'):
                # Chunk removal then individual removal, never remove the target.
                width=max(1,(len(best[role])-1)//2)
                while width:
                    for start in range(0,len(best[role])-1,width):
                        if attempts>=attempt_cap:status='attempt-budget';break
                        if time.monotonic()>=deadline:status='time-budget';break
                        proposed={key:list(value) for key,value in best.items()}
                        del proposed[role][start:min(start+width,len(proposed[role])-1)]
                        attempts+=1
                        observed=run_histories(proposed,command,bundle['provenance'],deadline)
                        if reproduces(bundle,observed,signature):
                            if attempts>=attempt_cap:status='attempt-budget';break
                            if time.monotonic()>=deadline:status='time-budget';break
                            attempts+=1
                            repeated=run_histories(proposed,command,bundle['provenance'],deadline)
                            if not reproduces(bundle,repeated,signature) or any(normalized(observed[key])!=normalized(repeated[key]) for key in observed):continue
                            best=proposed;improved=True;changes.append({'attempt':attempts,'role':role,'start':start,'count':width});break
                    if improved or status!='local-minimum':break
                    width=width//2 if width>1 else 0
                if improved or status!='local-minimum':break
            if status!='local-minimum':break
    except ToolFailure as error:
        failure=str(error);status='time-budget' if time.monotonic()>=deadline else 'tool-failure'
    return {'schema':1,'kind':'oracle-history','promotable':False,'oracle_history':{'schema':1,'roles':bundle['oracle_history']['roles'],'requests':best},'predicate':[list(row) for row in signature],'attempts':attempts,'status':status,'error':failure,'history':changes}


def replay_history(bundle,command,deadline,shrink_mode=None):
    requests,signature=validate_history(bundle)
    # Each repetition owns two new processes, not repetitions in contaminated sessions.
    for _ in range(2):
        observed=run_histories(requests,command,bundle['provenance'],deadline-2)
        if not reproduces(bundle,observed,signature,exact=True):raise ToolFailure('native-history-divergence-not-reproduced')
    result={'schema':1,'status':'reproduced','kind':'oracle-integrity','promotable':False,'fresh_repetitions':2,'predicate':[list(row) for row in signature]}
    if shrink_mode:
        result['shrink']=shrink_history(bundle,command,deadline-2,64 if shrink_mode=='pr' else 2000)
    return result
