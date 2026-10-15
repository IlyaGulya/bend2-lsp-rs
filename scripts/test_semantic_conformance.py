#!/usr/bin/env python3
"""Tool-logic tests only; semantic truth in the gate comes from the real compiler."""
import copy
import json
import io
import subprocess
import time
import zipfile
import re
import shutil
from pathlib import Path
import sys
import tempfile
import unittest

from semantic_conformance import ToolFailure, Worker, apply_manifest_requirements, check_manifest_assertions, compare, make_plan, normalized, semantic_structure, validate_plan, validate_source_provenance
from semantic_generate import FAMILIES, PROFILE, canonical, generate, layout_variant, remap_answer, validate_bounds
from semantic_install import SOURCE, authenticate_runtime, digest, instrument
from semantic_replay import load_bundle, predicate, reductions, safe_name
from collections import Counter
from semantic_shapes import source_depth


class ComparatorTests(unittest.TestCase):
    def setUp(self):
        # A normalized protocol specimen, not a golden/candidate semantic oracle.
        # Mutation sensitivity is independent of which compiler emitted a valid graph.
        site={'file':'main.bend','start':10,'end':16};use={'file':'main.bend','start':30,'end':36}
        self.answer={'schema':1,'id':'case','root':'main.bend','outcome':'valid','declarations':[{'site':site,'kind':'law','name':'target','target':site},{'site':{'file':'main.bend','start':40,'end':46},'kind':'def','name':'target','target':site}],'occurrences':[{'site':use,'role':'call','target':site,'binding':'global','status':'resolved'}]}
        self.answer['revisions']={'main.bend':1}
        for declaration in self.answer['declarations']:
            declaration['signatureOwner']=site.copy();declaration['bodyOwner']=self.answer['declarations'][1]['site'].copy()
        self.answer['occurrences'][0]['enclosing']=site.copy()

    def test_exact_reply_matches(self):self.assertEqual(compare(self.answer,copy.deepcopy(self.answer)),[])

    def test_missing_and_extra_edges_fail(self):
        for operation in ('missing','extra','retarget','binder','root','law','utf16','stale','body','signature','enclosing'):
            changed=copy.deepcopy(self.answer)
            if operation=='missing':changed['occurrences'].clear()
            elif operation=='extra':
                extra=copy.deepcopy(changed['occurrences'][0]);extra['site']['start']+=1;extra['site']['end']+=1;changed['occurrences'].append(extra)
            elif operation=='retarget':changed['occurrences'][0]['target']={'file':'other.bend','start':10,'end':16}
            elif operation=='binder':changed['occurrences'][0]['binding']='local'
            elif operation=='root':changed['root']='another.bend'
            elif operation=='law':changed['declarations'].pop(0)
            elif operation=='utf16':changed['occurrences'][0]['site']['start']+=1
            elif operation=='stale':changed['revisions']['main.bend']=2
            elif operation=='body':changed['declarations'][0]['bodyOwner']=changed['declarations'][0]['site']
            elif operation=='signature':changed['declarations'][0]['signatureOwner']=changed['declarations'][1]['site']
            elif operation=='enclosing':changed['occurrences'][0]['enclosing']=changed['declarations'][1]['site']
            with self.subTest(operation=operation):self.assertTrue(compare(self.answer,changed))

    def test_duplicate_record_is_protocol_failure(self):
        self.answer['occurrences'].append(copy.deepcopy(self.answer['occurrences'][0]))
        with self.assertRaises(ToolFailure):normalized(self.answer)

    def test_unknown_is_not_empty_pass(self):
        changed=copy.deepcopy(self.answer);changed['occurrences'][0]['target']=None;changed['occurrences'][0]['status']='uncertain'
        self.assertTrue(compare(self.answer,changed))

    def test_incomplete_or_rejected_cannot_match(self):
        for bad in ({'outcome':'compiler-error'},{'outcome':'valid','declarations':[]}):
            with self.assertRaises(ToolFailure):compare(self.answer,bad)

    def test_selected_base_law_body_is_not_omitted_or_collapsed(self):
        expected=copy.deepcopy(self.answer);site={'file':'@Base.bend','start':100,'end':112};body={'file':'@Base.bend','start':160,'end':172}
        expected['base_entities']=[{'site':site,'name':'U32.add_comm','kind':'law','target':site,'signatureOwner':site,'bodyOwner':body}]
        self.assertEqual(compare(expected,copy.deepcopy(expected)),[])
        for mutation in ('missing','extra','native-body','signature'):
            actual=copy.deepcopy(expected)
            if mutation=='missing':actual['base_entities']=[]
            elif mutation=='extra':
                record=copy.deepcopy(actual['base_entities'][0]);record['name']='another';record['site']={**site,'start':200,'end':212};actual['base_entities'].append(record)
            elif mutation=='native-body':actual['base_entities'][0]['bodyOwner']=None
            else:actual['base_entities'][0]['signatureOwner']=body
            with self.subTest(mutation=mutation):self.assertTrue(compare(expected,actual))

    def test_consumer_predicate_ignores_request_id_not_semantic_site(self):
        from semantic_stdio import ConsumerMismatch
        expected=[self.answer['declarations'][0]['site']];actual=[self.answer['declarations'][1]['site']]
        first=ConsumerMismatch('references',{'id':'a'},expected,actual)
        repeated=ConsumerMismatch('references',{'id':'b'},expected,actual)
        changed=ConsumerMismatch('references',{'id':'a'},expected,[{'file':'main.bend','start':70,'end':76}])
        self.assertEqual(first.predicate,repeated.predicate);self.assertNotEqual(first.predicate,changed.predicate)

    def test_invalid_offload_recovery_does_not_invent_call_edges(self):
        from semantic_conformance import check_manifest_assertions
        case={'files':{'main.bend':'target! (1)\n'},'recovery_assertions':{'no_call_at':'target! (1)'}}
        answer=copy.deepcopy(self.answer);answer['occurrences'][0]['site']={'file':'main.bend','start':0,'end':6}
        answer['occurrences'][0]['role']='value'
        check_manifest_assertions(case,'main.bend',answer,recovery=True)
        for role in ('call','offload'):
            answer['occurrences'][0]['role']=role
            with self.subTest(role=role),self.assertRaisesRegex(ToolFailure,'recovery-invented-call'):
                check_manifest_assertions(case,'main.bend',answer,recovery=True)


class ManifestRequirementTests(unittest.TestCase):
    def test_inline_witness_survives_absent_global_entry(self):
        case={'id':'inline','assertions':[{'use':'call'}],'expected_error':'incomplete-compiler-input','recovery_assertions':{'declaration':'incomplete'}}
        expected=copy.deepcopy(case)
        apply_manifest_requirements(case,{})
        self.assertEqual(case,expected)

    def test_global_and_inline_occurrence_witnesses_are_both_enforced(self):
        first={'use':'first','binding':'local','role':'value','target_file':'main.bend'}
        second={'use':'second','binding':'local','role':'type','target_file':'main.bend'}
        case={'id':'both','files':{'main.bend':'first second'},'assertions':[second]}
        apply_manifest_requirements(case,{'assertions':{'both':[first]}})
        target={'file':'main.bend','start':20,'end':21}
        answer={'occurrences':[{'site':{'file':'main.bend','start':0,'end':5},'role':'value','binding':'local','target':target},{'site':{'file':'main.bend','start':6,'end':12},'role':'type','binding':'local','target':target}]}
        check_manifest_assertions(case,'main.bend',answer)
        for index in (0,1):
            changed=copy.deepcopy(answer);changed['occurrences'][index]['target']=None
            with self.subTest(index=index),self.assertRaisesRegex(ToolFailure,'oracle-witness-target-not-established'):
                check_manifest_assertions(case,'main.bend',changed)

    def test_target_origin_witness_distinguishes_same_span_read_and_rebind(self):
        case={'files':{'main.bend':'a = x; a[0] <- y'},'assertions':[
            {'use':'a[0]','role':'value','binding':'local','target_file':'main.bend','target_use':'a = x','target_length':1},
            {'use':'a[0]','role':'binding','binding':'local','target_file':'main.bend','target_use':'a[0]','target_length':1},
        ]}
        old={'file':'main.bend','start':0,'end':1};new={'file':'main.bend','start':7,'end':8}
        answer={'occurrences':[{'site':new,'role':'value','binding':'local','target':old},{'site':new,'role':'binding','binding':'local','target':new}]}
        check_manifest_assertions(case,'main.bend',answer)
        for index,target in ((0,new),(1,old)):
            changed=copy.deepcopy(answer);changed['occurrences'][index]['target']=target
            with self.subTest(index=index),self.assertRaisesRegex(ToolFailure,'oracle-witness-target-origin-not-established'):
                check_manifest_assertions(case,'main.bend',changed)

    def test_global_and_inline_recovery_witnesses_are_both_enforced(self):
        case={'id':'both','files':{'main.bend':'def main(): missing() target! (1)'},'recovery_assertions':{'unknown':'missing','no_call_at':'target! (1)'}}
        apply_manifest_requirements(case,{'recovery_assertions':{'both':{'declaration':'main'}}})
        answer={'declarations':[{'site':{'file':'main.bend','start':4,'end':8},'kind':'def','name':'main'}],'occurrences':[{'site':{'file':'main.bend','start':12,'end':19},'status':'uncertain','target':None,'role':'value'}]}
        check_manifest_assertions(case,'main.bend',answer,recovery=True)
        for mutation,error in (('declaration','recovery-lost-declaration'),('unknown','recovery-lost-unknown'),('call','recovery-invented-call')):
            changed=copy.deepcopy(answer)
            if mutation=='declaration':changed['declarations']=[]
            elif mutation=='unknown':changed['occurrences']=[]
            else:changed['occurrences'].append({'site':{'file':'main.bend','start':22,'end':28},'role':'offload'})
            with self.subTest(mutation=mutation),self.assertRaisesRegex(ToolFailure,error):
                check_manifest_assertions(case,'main.bend',changed,recovery=True)

    def test_conflicting_single_value_requirements_fail_closed(self):
        for case,corpus in (
            ({'id':'conflict','expected_error':'inline'},{'expected_errors':{'conflict':'global'}}),
            ({'id':'conflict','recovery_assertions':{'declaration':'inline'}},{'recovery_assertions':{'conflict':{'declaration':'global'}}}),
        ):
            with self.subTest(case=case),self.assertRaisesRegex(ToolFailure,'conflicting-corpus'):
                apply_manifest_requirements(case,corpus)

    def test_source_provenance_binds_exact_bytes_and_requires_complete_identity(self):
        source='def main(): Type\r\n'
        provenance={'repository':'https://github.com/IlyaGulya/bend','commit':'4a976ca4291c77bf208c2b6f6ed7e290de1c0280','path':'tests/proof/example.bend','file':'example.bend','sha256':digest(source.encode())}
        case={'files':{'example.bend':source},'source_provenance':[provenance]}
        validate_source_provenance(case)
        changed=copy.deepcopy(case);changed['files']['example.bend']=source.replace('\r\n','\n')
        with self.assertRaisesRegex(ToolFailure,'corpus-source-provenance-mismatch'):validate_source_provenance(changed)
        for field in provenance:
            changed=copy.deepcopy(case);del changed['source_provenance'][0][field]
            with self.subTest(missing=field),self.assertRaisesRegex(ToolFailure,'invalid-corpus-source-provenance'):
                validate_source_provenance(changed)
        for field,value in (('commit','main'),('path','../example.bend'),('file','absent.bend'),('sha256',None)):
            changed=copy.deepcopy(case);changed['source_provenance'][0][field]=value
            with self.subTest(field=field),self.assertRaisesRegex(ToolFailure,'invalid-corpus-source-provenance'):
                validate_source_provenance(changed)


class GeneratorTests(unittest.TestCase):
    def test_exact_deterministic_profile(self):
        a=list(generate(5301));b=list(generate(5301))
        self.assertEqual(len(a),256);self.assertEqual(canonical(a),canonical(b))
        self.assertEqual(set(FAMILIES),{family for case in a for family in case['families']})
        self.assertEqual(len({case['id'] for case in a}),256)
        for case in a:validate_bounds(case)
        self.assertNotEqual(canonical(a),canonical(list(generate(5302))))
        self.assertEqual(len({case['input_sha256'] for case in a}),256)
        self.assertEqual(len({canonical(case['structural_shape']) for case in a}),256)
        self.assertEqual({case['structural_shape']['mode'] for case in a},set(range(6)))
        self.assertGreater(max(case['depth'] for case in a),min(case['depth'] for case in a))
        for case in a:
            self.assertGreaterEqual(case['depth'],source_depth(case['files']))
            self.assertLessEqual(case['depth'],8)
        def distribution(cases):
            return Counter((item['structural_shape']['mode'],len(item['structural_shape']['calls']),item['structural_shape']['chain'],item['structural_shape']['arity'],item['structural_shape']['capture']) for item in cases)
        self.assertNotEqual(distribution(a),distribution(list(generate(5302))))

    def test_structural_digest_ignores_cosmetic_identity_but_not_target_edges(self):
        original=ComparatorTests();original.setUp();answer=original.answer
        renamed=json.loads(json.dumps(answer))
        for record in renamed['declarations']+renamed['occurrences']:
            for field in ('site','target','signatureOwner','bodyOwner','enclosing'):
                if record.get(field):record[field]['start']+=100;record[field]['end']+=100
            if 'name' in record:record['name']='renamed'
        self.assertEqual(semantic_structure(answer),semantic_structure(renamed))
        retargeted=copy.deepcopy(answer);retargeted['occurrences'][0]['target']=retargeted['declarations'][1]['site']
        self.assertNotEqual(semantic_structure(answer),semantic_structure(retargeted))

    def test_layout_mapping_utf8_crlf(self):
        case={'id':'unicode','roots':['main.bend'],'files':{'main.bend':'# 🦀\r\ndef target() -> Type:\r\n  Type\r\n'},'revisions':{'main.bend':1}}
        variant,maps=layout_variant(case)
        old=case['files']['main.bend'].encode();new=variant['files']['main.bend'].encode()
        start=old.index(b'target');site={'file':'main.bend','start':start,'end':start+6}
        answer={'declarations':[{'site':site,'target':site.copy()}],'occurrences':[]}
        transformed=remap_answer(answer,maps)['declarations'][0]['site']
        self.assertEqual(new[transformed['start']:transformed['end']],b'target')

    def test_manifest_is_exact_and_pin_bound(self):
        plan=make_plan('pr',5301);validate_plan(plan)
        self.assertEqual(plan['counts']['projects'],256)
        changed=copy.deepcopy(plan);changed['planned'].pop()
        with self.assertRaises(ToolFailure):validate_plan(changed)

    def test_depth_and_size_fail_closed(self):
        case=next(generate());case['files']['main.bend']='x'*16385
        with self.assertRaises(ValueError):validate_bounds(case)


class ProvenanceAndReplayTests(unittest.TestCase):
    def test_compiler_pin_rejects_tampered_artifacts(self):
        from unittest.mock import patch
        import semantic_install as installer
        with tempfile.TemporaryDirectory() as temporary:
            copied=Path(temporary)/'compiler';shutil.copytree(SOURCE,copied)
            pin=json.loads((copied/'pin.json').read_text())
            with patch.object(installer,'SOURCE',copied):
                for name in pin['files']:
                    path=copied/name;original=path.read_bytes()
                    path.write_bytes(original+b'\ncorrupted')
                    with self.assertRaisesRegex(ValueError,'compiler provenance mismatch: '+re.escape(name)):
                        installer.install(check=True)
                    path.write_bytes(original)

    def test_compiler_identity_rejects_substituted_release_provenance(self):
        import semantic_install as installer
        pin=json.loads((SOURCE/'pin.json').read_text())
        for field,value in (
            ('id','bend-2.0.37-dev-content-v1'),
            ('compiler_version','2.0.37-dev'),
            ('repository','https://github.com/other/bend'),
            ('release_api','https://api.github.com/repos/bendlang/bend/releases/tags/v2.0.36'),
            ('source_commit','0'*40),
            ('source_archive',{'size':1,'sha256':'0'*64}),
        ):
            changed=copy.deepcopy(pin);changed[field]=value
            with self.subTest(field=field),self.assertRaisesRegex(ValueError,'official compiler pin mismatch: '+field):
                installer.validate_compiler_pin(changed)

    def test_self_consistent_substitute_compiler_is_not_official(self):
        from unittest.mock import patch
        import semantic_install as installer
        with tempfile.TemporaryDirectory() as temporary:
            copied=Path(temporary)/'compiler';shutil.copytree(SOURCE,copied)
            pin=json.loads((copied/'pin.json').read_text())
            for name in ('bend.ts','base.bend'):
                original=(copied/name).read_bytes()
                substituted=original+b'\nsubstituted'
                (copied/name).write_bytes(substituted)
                changed=copy.deepcopy(pin);changed['files'][name]=installer.digest(substituted)
                with patch.object(installer,'SOURCE',copied),self.assertRaisesRegex(ValueError,'compiler provenance mismatch: '+re.escape(name)):
                    installer.validate_compiler_pin(changed)
                (copied/name).write_bytes(original)

    def test_compiler_pin_tracks_product_source_identity(self):
        from unittest.mock import patch
        import semantic_install as installer
        pin=json.loads((SOURCE/'pin.json').read_text())
        official=json.loads(installer.OFFICIAL_PINS.read_text())
        with tempfile.TemporaryDirectory() as temporary:
            path=Path(temporary)/'official.json'
            for field,value in (('version','2.0.38'),('source_commit','0'*40)):
                changed=copy.deepcopy(official);changed[field]=value
                path.write_text(json.dumps(changed))
                with patch.object(installer,'OFFICIAL_PINS',path),self.assertRaisesRegex(ValueError,'official compiler pin mismatch'):
                    installer.validate_compiler_pin(pin)
            for group,name in (('source','bend2/bend.ts'),('resources','bend2/base.bend')):
                changed=copy.deepcopy(official)
                files=changed[group]['files'] if group=='source' else changed[group]
                files[name]=[1,'0'*64];path.write_text(json.dumps(changed))
                with patch.object(installer,'OFFICIAL_PINS',path),self.assertRaisesRegex(ValueError,'compiler provenance mismatch'):
                    installer.validate_compiler_pin(pin)

    def test_runtime_provenance_rejects_version_revision_and_release_substitution(self):
        from unittest.mock import patch
        import semantic_install as installer
        with tempfile.TemporaryDirectory() as temporary:
            copied=Path(temporary)/'compiler';shutil.copytree(SOURCE,copied)
            pin=json.loads((copied/'pin.json').read_text())
            original=json.loads((copied/'runtime-pin.json').read_text())
            for field,value in (('version','1.4.2'),('revision','0'*40),('release_api','https://api.github.com/repos/other/bun/releases/tags/bun-v1.4.0')):
                changed=copy.deepcopy(original);changed[field]=value
                (copied/'runtime-pin.json').write_text(json.dumps(changed))
                with patch.object(installer,'SOURCE',copied),self.assertRaisesRegex(ValueError,'runtime provenance mismatch'):
                    installer.validate_compiler_pin(pin)
            (copied/'runtime-pin.json').write_text(json.dumps(original))
            for field,value in (('version','1.4.2'),('revision','0'*40)):
                changed=copy.deepcopy(pin);changed['runtime'][field]=value
                with patch.object(installer,'SOURCE',copied),self.assertRaisesRegex(ValueError,'runtime provenance mismatch'):
                    installer.validate_compiler_pin(changed)

    def test_runtime_execution_rejects_wrong_version_or_full_revision(self):
        from unittest.mock import Mock, patch
        import semantic_install as installer
        pin=json.loads((SOURCE/'pin.json').read_text())
        for version,revision,message in (
            ('1.4.2',pin['runtime']['revision'],'runtime pin mismatch'),
            (pin['runtime']['version'],'0'*40,'runtime revision mismatch'),
        ):
            results=[Mock(stdout=version+'\n'),Mock(stdout=revision+'\n')]
            with patch.object(installer,'runtime_path',return_value='/authenticated/bun'),patch.object(installer.subprocess,'run',side_effect=results),self.assertRaisesRegex(ValueError,message):
                installer.install()

    def test_instrumentation_rejects_incompatible_compiler(self):
        with self.assertRaises(ValueError):instrument('wrong source')

    def test_modified_native_worker_invalidates_installation_and_cache(self):
        from unittest.mock import Mock, patch
        import semantic_install as installer
        names={'semantic_worker.ts':'worker.ts','semantic_pristine_worker.ts':'semantic_pristine_worker.ts','semantic_book_stream.ts':'semantic_book_stream.ts'}
        recorded={name:(installer.ROOT/'scripts'/name).read_text() for name in names}
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);(root/'scripts').mkdir()
            for name in names:(root/'scripts'/name).write_text(recorded[name])
            runtime=root/'bun';runtime.write_bytes(b'authenticated-runtime-test-specimen')
            runtime_pin=json.loads((SOURCE/'pin.json').read_text())['runtime']
            def runtime_result(command,**kwargs):
                return Mock(stdout=runtime_pin['version' if command[1]=='--version' else 'revision']+'\n')
            with patch.object(installer,'ROOT',root),patch.object(installer,'CACHE',root/'cache'),patch.object(installer,'runtime_path',return_value=str(runtime)),patch.object(installer.subprocess,'run',side_effect=runtime_result):
                destination,provenance=installer.install()
                for name,field in (('semantic_pristine_worker.ts','pristine_worker_sha256'),('semantic_book_stream.ts','book_stream_sha256')):
                    (destination/names[name]).write_text('tampered')
                    with self.assertRaisesRegex(ValueError,'stale'):installer.install(check=True)
                    installer.install()
                    source=root/'scripts'/name;source.write_text(source.read_text()+'\n')
                    changed,changed_provenance=installer.install()
                    self.assertNotEqual(destination,changed);self.assertNotEqual(provenance[field],changed_provenance[field])
                    source.write_text(recorded[name])

    def test_bundle_paths_fail_closed(self):
        for path in ('../escape','/absolute','a/../../b','a\\b'):
            with self.assertRaises(ToolFailure):safe_name(path)
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);(root/'original.json').write_text('{}');(root/'sha256.json').write_text('{"original.json":"wrong"}')
            with self.assertRaises(ToolFailure):load_bundle(root)

    def test_reductions_are_deterministic_whole_productions(self):
        case=next(generate());left=list(reductions(case));right=list(reductions(case))
        self.assertEqual(canonical(left),canonical(right));self.assertTrue(left)
        for _,candidate in left:self.assertIn('main.bend',candidate['files'])

    def test_zombie_leader_permission_error_requires_kernel_absence(self):
        from unittest.mock import Mock, patch
        from semantic_process import signal_group
        process=Mock(pid=12345);process.wait.return_value=0
        with patch('semantic_process.os.killpg',side_effect=[PermissionError(1,'zombie-only'),ProcessLookupError(3,'reaped')]) as kill:
            signal_group(process,9)
        process.wait.assert_called_once_with(timeout=.1);self.assertEqual(kill.call_count,2)

    def test_permission_error_with_live_leader_or_group_is_not_suppressed(self):
        from unittest.mock import Mock, patch
        from semantic_process import signal_group
        for leader in (None,0):
            process=Mock(pid=12345)
            if leader is None:process.wait.side_effect=subprocess.TimeoutExpired('worker',.1)
            else:process.wait.return_value=0
            with self.subTest(leader=leader),patch('semantic_process.os.killpg',side_effect=PermissionError(1,'inaccessible live group')):
                with self.assertRaises(PermissionError):signal_group(process,9)

    def test_reaped_leader_does_not_skip_signaling_surviving_group(self):
        from unittest.mock import Mock, patch
        from semantic_process import signal_group
        process=Mock(pid=12345);process.wait.return_value=0
        with patch('semantic_process.os.killpg',side_effect=[PermissionError(1,'zombie race'),None]) as kill:
            signal_group(process,9)
        self.assertEqual(kill.call_args_list[-1].args,(12345,9));self.assertEqual(kill.call_count,2)

    def test_real_process_timeout_is_bounded(self):
        worker=Worker([sys.executable,'-c','import time; time.sleep(30)'])
        try:
            with self.assertRaisesRegex(ToolFailure,'timeout'):worker.ask({'schema':1,'id':'x','root':'main.bend'},0.01)
        finally:worker.close()
        self.assertIsNotNone(worker.process.returncode)

    def test_real_process_incomplete_reply_fails(self):
        worker=Worker([sys.executable,'-c','import sys; sys.stdout.write("{");sys.stdout.flush()'])
        try:
            with self.assertRaises(ToolFailure):worker.ask({'schema':1,'id':'x','root':'main.bend'},2)
        finally:worker.close()
    def test_runtime_override_cannot_self_attest_version(self):
        archive=io.BytesIO()
        with zipfile.ZipFile(archive,'w') as package:package.writestr('bun-test/bun',b'authenticated release binary')
        data=archive.getvalue();artifact={'name':'bun-test.zip','sha256':digest(data)}
        with tempfile.TemporaryDirectory() as temporary:
            candidate=Path(temporary)/'bun';candidate.write_text('#!/bin/sh\necho 1.4.2\n');candidate.chmod(0o755)
            with self.assertRaisesRegex(ValueError,'authenticated release archive'):authenticate_runtime(candidate,data,artifact)
            candidate.write_bytes(b'authenticated release binary')
            self.assertEqual(authenticate_runtime(candidate,data,artifact),str(candidate.resolve()))
            with self.assertRaisesRegex(ValueError,'artifact checksum'):authenticate_runtime(candidate,data+b'corrupt',artifact)

    def test_reply_flood_is_bounded_and_killed(self):
        program='import sys,time;sys.stdin.readline();sys.stdout.write("{}\\n"*1000000);sys.stdout.flush();time.sleep(30)'
        worker=Worker([sys.executable,'-c',program])
        try:
            with self.assertRaises(ToolFailure):worker.ask({'schema':1,'id':'x','root':'main.bend'},2)
            self.assertLessEqual(worker.lines.qsize(),1)
        finally:worker.close()
        self.assertIsNotNone(worker.process.returncode)

    def test_leader_exit_does_not_escape_group_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            pidfile=Path(temporary)/'child.pid'
            child='import os,signal,time,pathlib;signal.signal(signal.SIGTERM,signal.SIG_IGN);pathlib.Path('+repr(str(pidfile))+').write_text(str(os.getpid()));time.sleep(30)'
            leader='import subprocess,sys,time,pathlib;subprocess.Popen([sys.executable,"-c",'+repr(child)+']);p=pathlib.Path('+repr(str(pidfile))+');exec("while not p.exists(): time.sleep(.01)")'
            worker=Worker([sys.executable,'-c',leader])
            try:
                worker.process.wait(timeout=3)
                child_pid=int(pidfile.read_text())
            finally:worker.close()
            deadline=time.monotonic()+2
            while True:
                status=subprocess.run(['ps','-o','stat=','-p',str(child_pid)],capture_output=True,text=True,check=False).stdout.strip()
                if not status or status.startswith('Z'):break
                if time.monotonic()>=deadline:self.fail('descendant survived exited session leader')
                time.sleep(.01)

    def test_homonymous_nested_binders_keep_distinct_shrink_identity(self):
        def answer(target):
            declaration={'file':'main.bend','start':4,'end':8}
            return {'root':'main.bend','outcome':'valid','revisions':{'main.bend':1},'declarations':[{'site':declaration,'target':declaration,'name':'main','kind':'def','signatureOwner':declaration.copy(),'bodyOwner':declaration.copy()}],'occurrences':[{'site':{'file':'main.bend','start':35,'end':36},'role':'value','binding':'local','status':'resolved','target':{'file':'main.bend','start':target,'end':target+1},'enclosing':declaration.copy()}]}
        source={'main.bend':' '*10+'x'+' '*9+'x'+' '*14+'x'}
        observed=predicate(answer(20),answer(10),source)
        swapped=predicate(answer(10),answer(20),source)
        self.assertNotEqual(observed,swapped)
        moved=answer(21)
        self.assertNotEqual(observed,predicate(moved,answer(10),source))


if __name__=='__main__':unittest.main()
