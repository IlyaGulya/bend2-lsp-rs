"""Real stdio counterexample replay; compiler validity is checked on every reduction."""
import copy
import time

from semantic_conformance import ToolFailure, canonical, compare
from semantic_install import digest
from semantic_stdio import ConsumerMismatch, run_stdio


def observe_consumer(bundle,compiler,binary,case,deadline,oracle_budget=None):
    selected=None;root=bundle['request']['root']
    for entry in case['roots']:
        if oracle_budget is not None:
            if oracle_budget['attempts']>=oracle_budget['limit']:raise ToolFailure('consumer-shrink-attempt-budget')
            oracle_budget['attempts']+=1
        request={'schema':1,'id':bundle['request']['id'],'root':entry,'files':case['files'],'disk_files':case.get('disk_files',case['files']),'revisions':case['revisions'],'inspect_base':case.get('inspect_base',[])}
        answer=compiler.ask(request,min(15,deadline-time.monotonic()))
        if answer['outcome']=='oracle-error':raise ToolFailure('consumer-oracle-error:'+answer.get('error',''))
        if answer['outcome']!='valid':return None
        if entry==root:selected=answer
    if selected is None:raise ToolFailure('consumer-root-removed')
    item=copy.deepcopy(bundle['item']);item['case']=case;item['input_sha256']=digest(canonical(case))
    try:
        run_stdio({'planned':[item]},{item['id']:selected},binary,deadline,selected_ids=(item['case_id'],))
    except ConsumerMismatch as error:return error,selected
    except StopIteration:return None
    except ToolFailure as error:
        if str(error).startswith('stdio-empty-selected-consumer-case'):return None
        raise
    return None


def replay_consumer(bundle,compiler,binary,deadline):
    expected=bundle['consumer'].get('predicate')
    if not expected:raise ToolFailure('recorded-consumer-failure-has-no-semantic-predicate')
    for _ in range(2):
        observed=observe_consumer(bundle,compiler,binary,bundle['item']['case'],deadline)
        if observed is None:raise ToolFailure('consumer-mismatch-not-reproduced')
        failure,oracle=observed
        if compare(bundle['oracle'],oracle):raise ToolFailure('consumer-replay-compiler-semantics-changed')
        if failure.predicate!=expected:raise ToolFailure('consumer-mismatch-changed-or-nondeterministic')
    return {'schema':1,'status':'reproduced','candidate_kind':'lsp','predicate':expected}


def shrink_consumer(bundle,compiler,binary,deadline,attempt_cap):
    from semantic_replay import reductions
    best=copy.deepcopy(bundle['item']['case']);expected=bundle['consumer']['predicate']
    budget={'attempts':0,'limit':attempt_cap};history=[];seen={digest(canonical(best))};status='local-minimum';error=None
    try:
        while budget['attempts']<attempt_cap and time.monotonic()<deadline:
            improved=False
            for label,candidate in reductions(best):
                if bundle['request']['root'] not in candidate['roots']:continue
                checksum=digest(canonical(candidate))
                if checksum in seen:continue
                seen.add(checksum)
                observed=observe_consumer(bundle,compiler,binary,candidate,deadline,budget)
                if observed is None or observed[0].predicate!=expected:continue
                best=candidate;history.append({'attempt':budget['attempts'],'reduction':label,'sha256':checksum});improved=True;break
            if not improved:break
    except (ToolFailure,OSError,ValueError,KeyError,TypeError,IndexError,UnicodeError) as failure:
        error=str(failure);status='attempt-budget' if 'attempt-budget' in error else 'time-budget' if time.monotonic()>=deadline else 'tool-failure'
    if status=='local-minimum':
        if budget['attempts']>=attempt_cap:status='attempt-budget'
        elif time.monotonic()>=deadline:status='time-budget'
    best['consumer_profile']=best.get('consumer_profile',bundle['item']['case_id'])
    return {'schema':1,'case':best,'predicate':expected,'status':status,'attempts':budget['attempts'],'history':history,'error':error,'consumer':bundle['consumer']}
