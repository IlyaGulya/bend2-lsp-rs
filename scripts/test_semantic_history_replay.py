"""Process orchestration controls, not handcrafted compiler semantic truth."""
import base64
import copy
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from semantic_conformance import ToolFailure, canonical, request_for
from semantic_history_replay import replay_history, run_histories, shrink_history, validate_history
from semantic_install import digest


CONTROL='''import json,sys,os
state=0
for line in sys.stdin:
 r=json.loads(line)
 if r['id']=='seed':state=1
 outcome='oracle-error' if r['id']=='broken' else 'compiler-error' if r['id']=='invalid' else 'valid'
 a={'schema':1,'id':r['id'],'root':r['root'],'outcome':outcome,'provenance':{'control':1},'revisions':{'main.bend':state},'declarations':[],'occurrences':[],'metrics':{'pid':os.getpid()}}
 print(json.dumps(a),flush=True)
'''


def fixture():
    case={'files':{'main.bend':'main = 0'},'roots':['main.bend'],'revisions':{'main.bend':0}}
    item={'id':'target','root':'main.bend','case':case,'input_sha256':digest(canonical(case))}
    target=request_for(item)
    def request(identity):return dict(copy.deepcopy(target),id=identity)
    def answer(revision):return {'schema':1,'id':'target','root':'main.bend','outcome':'valid','provenance':{'control':1},'revisions':{'main.bend':revision},'declarations':[],'occurrences':[]}
    oracle=answer(1);candidate=answer(0)
    return {'item':item,'request':target,'provenance':{'control':1},'oracle':oracle,'candidate':candidate,'raw_replies_base64':{'reverse_oracle':base64.b64encode(canonical(oracle)).decode(),'forward_oracle':base64.b64encode(canonical(candidate)).decode()},'oracle_history':{'schema':1,'roles':{'oracle':'reverse','candidate':'forward'},'requests':{'oracle':[request('invalid'),request('seed'),target],'candidate':[target]}},'diff':[]}


class NativeHistoryReplayTests(unittest.TestCase):
    def setUp(self):
        self.directory=tempfile.TemporaryDirectory();self.addCleanup(self.directory.cleanup)
        script=Path(self.directory.name)/'control.py';script.write_text(CONTROL)
        self.command=[sys.executable,str(script)]

    def test_two_independent_sessions_allow_compiler_error_prefix(self):
        bundle=fixture();requests,_=validate_history(bundle)
        observed=run_histories(requests,self.command,bundle['provenance'],time.monotonic()+5)
        self.assertEqual(observed['oracle']['revisions'],{'main.bend':1})
        self.assertEqual(observed['candidate']['revisions'],{'main.bend':0})
        self.assertNotEqual(observed['oracle']['metrics']['pid'],observed['candidate']['metrics']['pid'])

    def test_reproduction_uses_four_fresh_processes(self):
        bundle=fixture();pids=[]
        from semantic_history_replay import run_histories as actual
        def observe(*args):
            result=actual(*args);pids.extend(answer['metrics']['pid'] for answer in result.values());return result
        with patch('semantic_history_replay.run_histories',side_effect=observe):
            result=replay_history(bundle,self.command,time.monotonic()+10)
        self.assertEqual(len(set(pids)),4)
        self.assertFalse(result['promotable'])

    def test_oracle_error_prefix_is_not_a_reproduction(self):
        bundle=fixture();bundle['oracle_history']['requests']['oracle'][0]['id']='broken'
        with self.assertRaisesRegex(ToolFailure,'oracle-error'):
            replay_history(bundle,self.command,time.monotonic()+5)

    def test_shrink_removes_irrelevant_prefix_not_state_seed(self):
        bundle=fixture();result=shrink_history(bundle,self.command,time.monotonic()+10,64)
        self.assertEqual([r['id'] for r in result['oracle_history']['requests']['oracle']],['seed','target'])
        self.assertEqual(result['oracle_history']['requests']['candidate'],[bundle['request']])
        self.assertFalse(result['promotable'])

    def test_zero_attempt_budget_preserves_original(self):
        bundle=fixture();result=shrink_history(bundle,self.command,time.monotonic()+5,0)
        self.assertEqual(result['status'],'attempt-budget')
        self.assertEqual(result['oracle_history'],bundle['oracle_history'])

    def test_missing_full_history_and_changed_raw_are_rejected(self):
        bundle=fixture();del bundle['oracle_history']
        with self.assertRaisesRegex(ToolFailure,'hash-only'):validate_history(bundle)
        bundle=fixture();bundle['candidate']['revisions']['main.bend']=8
        with self.assertRaisesRegex(ToolFailure,'raw-mismatch'):validate_history(bundle)

    def test_target_and_resource_boundaries(self):
        bundle=fixture();bundle['oracle_history']['requests']['oracle'].reverse()
        with self.assertRaisesRegex(ToolFailure,'target-not-last'):validate_history(bundle)
        bundle=fixture();bundle['oracle_history']['requests']['oracle'][0]['files']={'../escape':'bad'}
        with self.assertRaisesRegex(ToolFailure,'unsafe-bundle-path'):validate_history(bundle)


if __name__=='__main__':unittest.main()
