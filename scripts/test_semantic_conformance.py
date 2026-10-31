#!/usr/bin/env python3
"""Tool-logic tests only; semantic truth in the gate comes from the real compiler."""
import copy
import base64
from contextlib import closing
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

from semantic_conformance import RawHistorySpool, ReverseHistory, SpoolAnswers, ToolFailure, Worker, apply_manifest_requirements, check_manifest_assertions, compare, execute, make_plan, normalized, request_for, semantic_structure, validate_plan, validate_source_provenance
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

    def test_reordering_records_does_not_hide_semantic_differences(self):
        changed=copy.deepcopy(self.answer);changed['declarations'].reverse()
        self.assertEqual(compare(self.answer,changed),[])
        changed['declarations'][0]['bodyOwner']=None
        difference=compare(self.answer,changed)
        self.assertEqual([row['class'] for row in difference],['declarations'])
        self.assertEqual(len(difference[0]['missing']),1)
        self.assertEqual(len(difference[0]['extra']),1)

    def test_structure_still_rejects_malformed_base_records(self):
        base={'site':{'file':'@Base.bend','start':100,'end':112},'name':'Base.law','kind':'law','target':None,'signatureOwner':None,'bodyOwner':None}
        for mutation in ('duplicate','owner','site'):
            changed=copy.deepcopy(self.answer);changed['base_entities']=[copy.deepcopy(base)]
            if mutation=='duplicate':changed['base_entities'].append(copy.deepcopy(base))
            elif mutation=='owner':del changed['base_entities'][0]['signatureOwner']
            else:changed['base_entities'][0]['site']['end']=100
            with self.subTest(mutation=mutation),self.assertRaises(ToolFailure):
                semantic_structure(changed)

    def test_stdio_offsets_keep_non_lf_separators_on_the_same_line(self):
        from semantic_stdio import byte_offset
        source='# \v\f\x85\u2028\u2029 text\n\U0001d11etarget'
        self.assertEqual(byte_offset(source,{'line':1,'character':2}),22)

    def test_stdio_base_target_requires_authenticated_pinned_source(self):
        from semantic_stdio import BaseDocument, ConsumerMismatch, pinned_base_source
        source=pinned_base_source()
        with tempfile.TemporaryDirectory() as workspace_dir,tempfile.TemporaryDirectory() as base_dir:
            workspace=Path(workspace_dir);(workspace/'main.bend').write_text('import Base\n',newline='')
            base=Path(base_dir)/'Base.bend';base.write_bytes(source)
            start=source.index(b'def U32.add')+len(b'def ');end=start+len(b'U32.add')
            offset=source.rfind(b'\n',0,start)+1
            selection={'start':{'line':source[:start].count(b'\n'),'character':start-offset},
                'end':{'line':source[:start].count(b'\n'),'character':end-offset}}
            files={'main.bend':'import Base\n'}
            document=BaseDocument({'id':'base-target'},workspace,files,source)
            self.assertEqual(document.site(base.as_uri(),selection),
                {'file':'@Base.bend','start':start,'end':end})
            for uri in (base.as_uri().replace('file://','file://foreign.example'),
                    base.as_uri()+'?different-document',base.as_uri()+'#different-document'):
                with self.subTest(uri=uri),self.assertRaises(ConsumerMismatch) as rejected:
                    BaseDocument({'id':'base-target'},workspace,files,source).site(uri,selection)
                self.assertEqual(rejected.exception.category,'stdio-base-target-uri')
            base.write_bytes(b'def U32.add(x: U32, y: U32) -> U32:\n  x\n')
            with self.assertRaises(ConsumerMismatch) as rejected:
                BaseDocument({'id':'base-target'},workspace,files,source).site(base.as_uri(),selection)
            self.assertEqual(rejected.exception.category,'stdio-base-target-not-pinned-source')
            (workspace/'Base.bend').write_bytes(source)
            with self.assertRaises(ConsumerMismatch) as rejected:
                BaseDocument({'id':'base-target'},workspace,files,source).site((workspace/'Base.bend').as_uri(),selection)
            self.assertEqual(rejected.exception.category,'stdio-base-target-inside-fixture')

    def test_compact_layout_fingerprint_preserves_complete_semantic_identity(self):
        from semantic_conformance import _layout_fingerprint
        text='import lib as Alias\r\ndef x(x: Type):\r\n  Alias.target(x)\r\n'
        library='def target(x: Type):\r\n  x\r\n'
        def site(source,token,start=0,file='main.bend'):
            offset=source.encode().index(token.encode(),start)
            return {'file':file,'start':offset,'end':offset+len(token.encode())}
        owner=site(text,'x',text.index('def '));binder=site(text,'x',owner['end'])
        alias=site(text,'Alias');use=site(text,'Alias.target',alias['end'])
        target=site(library,'target',file='lib.bend')
        case={'id':'layout-identity','files':{'main.bend':text,'lib.bend':library},'roots':['main.bend'],'revisions':{'main.bend':1,'lib.bend':7}}
        _,mapping=layout_variant(case)
        base_site={'file':'@Base.bend','start':100,'end':112}
        answer={'outcome':'valid','root':'main.bend','revisions':case['revisions'],
            'declarations':[
                {'site':owner,'kind':'def','name':'x','target':owner,'signatureOwner':owner,'bodyOwner':owner},
                {'site':binder,'kind':'local','name':'x','target':binder,'signatureOwner':owner,'bodyOwner':None},
                {'site':alias,'kind':'alias','name':'Alias.target','target':target,'signatureOwner':None,'bodyOwner':None}],
            'occurrences':[
                {'site':use,'role':'call','target':target,'binding':'global','status':'resolved','enclosing':owner},
                {'site':site(text,'x',use['end']),'role':'value','target':binder,'binding':'local','status':'resolved','enclosing':owner}],
            'base_entities':[{'site':base_site,'name':'Base.law','kind':'law','target':base_site,'signatureOwner':base_site,'bodyOwner':None}]}
        before=copy.deepcopy(answer)
        fingerprint=_layout_fingerprint(answer,normalized(answer),mapping)
        transformed=remap_answer(answer,mapping)
        self.assertEqual(fingerprint,digest(canonical(normalized(transformed))))
        self.assertEqual(answer,before)
        for mutation in ('revision','target','owner','name','base-owner'):
            changed=copy.deepcopy(transformed)
            if mutation=='revision':changed['revisions']['lib.bend']+=1
            elif mutation=='target':changed['occurrences'][1]['target']=changed['declarations'][0]['site']
            elif mutation=='owner':changed['occurrences'][0]['enclosing']=None
            elif mutation=='name':changed['declarations'][2]['name']='Another.target'
            else:changed['base_entities'][0]['bodyOwner']=base_site
            with self.subTest(mutation=mutation):
                self.assertNotEqual(fingerprint,digest(canonical(normalized(changed))))

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

    def test_reaped_leader_waits_for_orphan_group_kernel_absence(self):
        from unittest.mock import Mock, patch
        from semantic_process import signal_group
        process=Mock(pid=12345);process.wait.return_value=0
        inaccessible=iter([True,True,False]);absent=False
        def kernel_signal(_pid,_signal):
            nonlocal absent
            if next(inaccessible):
                raise PermissionError(1,'orphan group awaiting reaping')
            absent=True
            raise ProcessLookupError(3,'group absent')
        with patch('semantic_process.os.killpg',side_effect=kernel_signal), \
                patch('semantic_process.time.monotonic',return_value=0), \
                patch('semantic_process.time.sleep'):
            signal_group(process,9)
        self.assertTrue(absent,'cleanup must observe kernel group absence')

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

    def test_reply_deadline_tracks_arrival_not_deferred_consumption(self):
        import threading
        from unittest.mock import patch
        request={'schema':1,'id':'x','root':'main.bend'}
        program='import sys;sys.stdin.readline();print(\'{"schema":1,"id":"x","root":"main.bend","outcome":"valid"}\',flush=True);sys.stdin.read()'
        for arrival in (100,102):
            with self.subTest(arrival=arrival):
                worker=Worker([sys.executable,'-c',program]);ready=threading.Event();clock=[100]
                original_put=worker.lines.put_nowait
                def publish(packet):
                    original_put(packet);ready.set()
                def now():
                    return arrival if threading.current_thread() is worker.reader else clock[0]
                try:
                    with patch.object(worker.lines,'put_nowait',side_effect=publish),patch('semantic_conformance.time.monotonic',side_effect=now):
                        pending=worker.start(request,1)
                        self.assertTrue(ready.wait(5),'real child response did not reach the bounded pipe')
                        clock[0]=103
                        if arrival==100:self.assertEqual(worker.receive(pending)['outcome'],'valid')
                        else:
                            with self.assertRaisesRegex(ToolFailure,'worker-timeout'):worker.receive(pending)
                finally:worker.close()

    def test_real_process_incomplete_reply_fails(self):
        worker=Worker([sys.executable,'-c','import sys; sys.stdout.write("{");sys.stdout.flush()'])
        try:
            with self.assertRaises(ToolFailure):worker.ask({'schema':1,'id':'x','root':'main.bend'},2)
        finally:worker.close()
    def test_runtime_override_cannot_self_attest_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            archive=Path(temporary)/'runtime.zip'
            with zipfile.ZipFile(archive,'w') as package:package.writestr('bun-test/bun',b'authenticated release binary')
            artifact={'name':'bun-test.zip','sha256':digest(archive.read_bytes())}
            candidate=Path(temporary)/'bun';candidate.write_text('#!/bin/sh\necho 1.4.2\n');candidate.chmod(0o755)
            with self.assertRaisesRegex(ValueError,'authenticated release archive'):authenticate_runtime(candidate,archive,artifact)
            candidate.write_bytes(b'authenticated release binary')
            self.assertEqual(authenticate_runtime(candidate,archive,artifact),str(candidate.resolve()))
            with archive.open('ab') as stream:stream.write(b'corrupt')
            with self.assertRaisesRegex(ValueError,'artifact checksum'):authenticate_runtime(candidate,archive,artifact)

    def test_runtime_archive_requires_the_exact_pinned_member(self):
        with tempfile.TemporaryDirectory() as temporary:
            archive=Path(temporary)/'runtime.zip';candidate=Path(temporary)/'bun'
            candidate.write_bytes(b'authenticated release binary')
            with zipfile.ZipFile(archive,'w') as package:package.writestr('other-platform/bun',candidate.read_bytes())
            artifact={'name':'bun-test.zip','sha256':digest(archive.read_bytes())}
            with self.assertRaises(KeyError):authenticate_runtime(candidate,archive,artifact)

    def test_runtime_archive_member_integrity_is_checked_while_streaming(self):
        with tempfile.TemporaryDirectory() as temporary:
            archive=Path(temporary)/'runtime.zip';candidate=Path(temporary)/'bun'
            candidate.write_bytes(b'authenticated release binary')
            with zipfile.ZipFile(archive,'w',compression=zipfile.ZIP_STORED) as package:
                package.writestr('bun-test/bun',candidate.read_bytes())
            with zipfile.ZipFile(archive) as package:
                member=package.getinfo('bun-test/bun')
                offset=member.header_offset+30+len(member.filename.encode())+len(member.extra)
            with archive.open('r+b') as stream:stream.seek(offset);stream.write(b'X')
            artifact={'name':'bun-test.zip','sha256':digest(archive.read_bytes())}
            with self.assertRaisesRegex(zipfile.BadZipFile,'CRC'):
                authenticate_runtime(candidate,archive,artifact)

    def test_runtime_authentication_checks_multichunk_member_and_executable_tails(self):
        with tempfile.TemporaryDirectory() as temporary:
            archive=Path(temporary)/'runtime.zip';candidate=Path(temporary)/'bun'
            tail=b'authenticated-tail\n'
            with candidate.open('wb') as stream:
                for _ in range(12):stream.write(bytes(range(256))*1024)
                stream.write(tail)
            with zipfile.ZipFile(archive,'w',compression=zipfile.ZIP_DEFLATED) as package:
                package.write(candidate,'bun-test/bun')
            artifact={'name':'bun-test.zip','sha256':digest(archive.read_bytes())}
            self.assertEqual(authenticate_runtime(candidate,archive,artifact),str(candidate.resolve()))
            with candidate.open('r+b') as stream:stream.seek(-len(tail),2);stream.write(b'X')
            with self.assertRaisesRegex(ValueError,'authenticated release archive'):
                authenticate_runtime(candidate,archive,artifact)
            with candidate.open('r+b') as stream:
                stream.seek(-len(tail),2);stream.write(tail);stream.truncate(stream.tell()-1)
            with self.assertRaisesRegex(ValueError,'authenticated release archive'):
                authenticate_runtime(candidate,archive,artifact)
            with archive.open('r+b') as stream:stream.seek(-1,2);stream.write(b'X')
            with self.assertRaisesRegex(ValueError,'artifact checksum'):
                authenticate_runtime(candidate,archive,artifact)

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


class ReverseHistoryFailureTests(unittest.TestCase):
    # Real pipes/processes exercise cancellation and late failure boundaries.
    # These protocol specimens are not a semantic oracle or native parity proof.
    def items(self):
        return [{'id':name,'case_id':name,'root':'main.bend','case':{'files':{'main.bend':'def main(): Type\n'},'revisions':{'main.bend':1}}} for name in ('a','b','c')]

    def history(self,program,items=None):
        directory=tempfile.TemporaryDirectory();self.addCleanup(directory.cleanup)
        spool=RawHistorySpool(directory.name);self.addCleanup(spool.close)
        items=self.items() if items is None else items
        self.forward_raw={}
        for item in items:
            specimen={'schema':1,'id':item['id'],'root':'main.bend','outcome':'valid','provenance':{},'revisions':{'main.bend':1},'declarations':[],'occurrences':[]}
            raw=b' '+canonical(specimen)+b'   ';self.forward_raw[item['id']]=raw
            spool.append('forward',item,raw,digest(canonical(normalized(specimen))))
        started=time.monotonic()
        history=ReverseHistory([sys.executable,'-c',program],items,{},started+4,started,spool)
        self.addCleanup(history.close)
        return history

    def valid_program(self,tail=''):
        return ("import json,sys\n"
            "for index,line in enumerate(sys.stdin):\n"
            " request=json.loads(line)\n"
            " reply={'schema':1,'id':request['id'],'root':'main.bend','outcome':'valid','provenance':{},'revisions':{'main.bend':1},'declarations':[],'occurrences':[]}\n"
            +tail+
            " print('  '+json.dumps(reply)+'  ',flush=True)\n")

    def test_last_reverse_error_preserves_original_reply_and_cannot_pass_partial(self):
        history=self.history(self.valid_program(" if index==2:reply.update(outcome='oracle-error',error='original-tail-failure')\n"))
        with self.assertRaisesRegex(ToolFailure,'original-tail-failure') as caught:
            history.finish({})
        evidence=caught.exception.history_failure
        self.assertEqual(evidence['item']['id'],'a')
        self.assertEqual(json.loads(evidence['raw'])['error'],'original-tail-failure')
        self.assertEqual([row['id'] for row in history.completed],['c','b'])
        self.assertFalse(history.thread.is_alive())
        self.assertIsNotNone(history.worker.process.returncode)

    def test_late_hash_mismatch_is_not_hidden_by_complete_reverse_count(self):
        history=self.history(self.valid_program(" if index==2:reply['revisions']['main.bend']=2\n"))
        specimen={'root':'main.bend','outcome':'valid','revisions':{'main.bend':1},'declarations':[],'occurrences':[]}
        expected=digest(canonical(normalized(specimen)))
        with self.assertRaisesRegex(ToolFailure,'oracle-state-leak') as caught:
            history.finish(dict.fromkeys(('a','b','c'),expected))
        evidence=caught.exception.history_failure
        self.assertEqual(evidence['item']['id'],'a')
        self.assertEqual(evidence['oracle']['revisions'],{'main.bend':2})
        self.assertEqual(evidence['forward']['revisions'],{'main.bend':1})
        self.assertTrue(evidence['raw'].startswith(b'  {'));self.assertTrue(evidence['raw'].endswith(b'}  '))
        self.assertEqual(evidence['forward_raw'],self.forward_raw['a'])
        self.assertEqual([row['id'] for row in evidence['prior_histories']['reverse']],['c','b'])
        self.assertEqual(evidence['prior_histories']['forward'],[])
        self.assertEqual([row['id'] for row in history.completed],['c','b','a'])
        self.assertTrue(history.metrics['complete'])
        self.assertEqual(evidence['oracle_history'],{'schema':1,'roles':{'oracle':'reverse','candidate':'forward'},'requests':{'oracle':[request_for(item) for item in reversed(self.items())],'candidate':[request_for(self.items()[0])]}})

    def test_settled_intersection_survives_a_later_reverse_native_error(self):
        history=self.history(self.valid_program(" if index==1:reply['revisions']['main.bend']=2\n if index==2:reply.update(outcome='oracle-error',error='original-tail-failure')\n"))
        with self.assertRaisesRegex(ToolFailure,'original-tail-failure'):history.finish({})
        specimen={'root':'main.bend','outcome':'valid','revisions':{'main.bend':1},'declarations':[],'occurrences':[]}
        failures=list(history.divergences({'b':digest(canonical(normalized(specimen)))}))
        self.assertEqual([evidence['item']['id'] for evidence in failures],['b'])
        self.assertEqual(failures[0]['forward_raw'],self.forward_raw['b'])
        self.assertEqual(history.failure['oracle']['error'],'original-tail-failure')

    def test_settled_intersection_does_not_invent_missing_forward_cases(self):
        history=self.history(self.valid_program(" reply['revisions']['main.bend']=2\n"))
        history.thread.join(timeout=3)
        specimen={'root':'main.bend','outcome':'valid','revisions':{'main.bend':1},'declarations':[],'occurrences':[]}
        failures=list(history.divergences({'b':digest(canonical(normalized(specimen)))}))
        self.assertEqual([evidence['item']['id'] for evidence in failures],['b'])

    def test_eof_keeps_truncated_original_bytes(self):
        history=self.history("import sys;sys.stdin.readline();sys.stdout.write('{original-truncated');sys.stdout.flush()")
        with self.assertRaisesRegex(ToolFailure,'truncated-reply') as caught:
            history.finish({})
        self.assertEqual(caught.exception.history_failure['raw'],b'{original-truncated')
        self.assertFalse(history.thread.is_alive())

    def test_forward_cancellation_does_not_erase_a_native_error_already_in_the_pipe(self):
        with tempfile.TemporaryDirectory() as directory:
            ready=Path(directory)/'emitted'
            program=("import json,pathlib,sys,time;request=json.loads(sys.stdin.readline());"
                "print(json.dumps({'schema':1,'id':request['id'],'root':'main.bend','outcome':'oracle-error','provenance':{},'error':'original-racing-failure'}),flush=True);"
                "pathlib.Path("+repr(str(ready))+").write_text('emitted');time.sleep(30)")
            history=self.history(program)
            deadline=time.monotonic()+2
            while not ready.exists() and history.failure is None and time.monotonic()<deadline:time.sleep(.001)
            self.assertTrue(ready.exists() or history.failure is not None)
            history.cancel();history.close()
            with self.assertRaisesRegex(ToolFailure,'original-racing-failure') as caught:history.check()
            self.assertEqual(json.loads(caught.exception.history_failure['raw'])['error'],'original-racing-failure')

    def test_early_forward_cancellation_unblocks_inflight_reverse_and_reaps_it(self):
        for phase in ('write','reply'):
            with self.subTest(phase=phase),tempfile.TemporaryDirectory() as directory:
                ready=Path(directory)/'inflight'
                items=self.items()
                if phase=='write':
                    for item in items:item['case']['files']['main.bend']='x'*140000
                consume="sys.stdin.readline();" if phase=='reply' else ""
                history=self.history("import pathlib,sys,time;"+consume+"pathlib.Path("+repr(str(ready))+").write_text('ready');time.sleep(30)",items)
                deadline=time.monotonic()+2
                while not ready.exists() and time.monotonic()<deadline:time.sleep(.005)
                self.assertTrue(ready.exists(),'reverse request did not start')
                self.assertTrue(history.worker.awaiting)
                started=time.monotonic();history.close()
                self.assertLess(time.monotonic()-started,3)
                self.assertFalse(history.thread.is_alive())
                self.assertIsNotNone(history.worker.process.returncode)
                self.assertIsNone(history.failure,'intentional cancellation replaced the forward failure')
                cutoff=history.cleanup_deadline;history.close()
                self.assertEqual(history.cleanup_deadline,cutoff)


class ExecuteHistoryEvidenceTests(unittest.TestCase):
    # These stateful subprocess controls prove evidence/orchestration behavior,
    # not compiler semantics. Only native smoke establishes semantic truth.
    def exercise_failure(self,mode):
        from unittest.mock import patch
        import semantic_conformance as conformance
        temporary=tempfile.TemporaryDirectory();self.addCleanup(temporary.cleanup)
        root=Path(temporary.name);artifacts=root/'artifacts'
        provenance={'pin_sha256':'protocol-specimen'}
        items=[]
        for name in ('bad','a','b','c'):
            case={'files':{'main.bend':'def main(): Type\n'},'revisions':{'main.bend':1},'families':[]}
            if mode=='manifest' and name=='a':case['assertions']=[{'use':'absent','role':'call','binding':'global','target_file':'main.bend'}]
            items.append({'id':name,'case_id':name,'root':'main.bend','case':case,'facet':'compiler-error' if name=='bad' else 'valid','variant':'original','input_sha256':digest(canonical(case))})
        plan={'schema':1,'profile':'protocol-specimen','seed':0,'planned':items,'counts':{'projects':0},**{name:'specimen' for name in ('manifest_sha256','pin_sha256','profile_sha256','corpus_sha256')}}
        common=("import json,sys\n"
            "seen=[]\n"
            "for line in sys.stdin:\n"
            " request=json.loads(line);key=request['id'];seen.append(key)\n"
            " reply={'schema':1,'id':key,'root':request['root'],'outcome':'valid','provenance':"+repr(provenance)+",'revisions':{'main.bend':1},'declarations':[],'occurrences':[]}\n")
        (root/'worker.ts').write_text(common+
            " reverse=seen[0]=='c'\n"
            " if key=='bad':reply.update(outcome='compiler-error',error='expected-prefix-rejection')\n"
            " if reverse and key=='a':reply['revisions']['main.bend']=2\n"+
            (" if not reverse and key=='b':reply.update(outcome='oracle-error',error='original-forward-native-error')\n" if mode=='native-error' else "")+
            " print('  '+json.dumps(reply)+'   ',flush=True)\n")
        candidate=root/'candidate'
        candidate_tail=''
        if mode=='mismatch':candidate_tail=" if key=='a':reply['revisions']['main.bend']=3\n"
        elif mode in ('candidate-error','archive-error'):candidate_tail=" if key=='a':reply.update(outcome='oracle-error',error='original-candidate-error')\n"
        elif mode=='malformed':candidate_tail=" if key=='a':sys.stdout.write('{candidate-original');sys.stdout.flush();sys.exit()\n"
        candidate.write_text('#!'+sys.executable+'\n'+common+candidate_tail+" print(' '+json.dumps(reply)+'  ',flush=True)\n")
        candidate.chmod(0o755)
        owners=[]
        def completed_reverse(*args):
            history=ReverseHistory(*args);owners.append(history)
            self.addCleanup(history.close)
            history.thread.join(timeout=2)
            self.assertFalse(history.thread.is_alive(),'protocol specimen did not settle')
            self.assertIsNone(history.failure)
            return history
        class BudgetedSpool(RawHistorySpool):
            def append(self,history,item,raw,fingerprint):
                if mode=='budget' and history=='forward' and item['id']=='a':
                    with patch.dict(conformance.GATE_PROFILE,{'artifact_bytes':self.path.stat().st_size}):
                        return super().append(history,item,raw,fingerprint)
                return super().append(history,item,raw,fingerprint)
        original_save=conformance.save_failure;original_spools=[]
        def archive_or_save(*args,**kwargs):
            if mode=='archive-error' and kwargs.get('oracle_history') is not None:
                original_spools.append(owners[0].spool.path.read_bytes())
                raise OSError('original-history-archive-write-error')
            return original_save(*args,**kwargs)
        with patch.object(conformance,'validate_plan'),patch.object(conformance,'install',return_value=(root,provenance)),patch.object(conformance,'native_worker_command',side_effect=lambda runtime,installation:[runtime,str(installation/'worker.ts')]),patch.object(conformance,'ReverseHistory',side_effect=completed_reverse),patch.object(conformance,'RawHistorySpool',BudgetedSpool),patch.object(conformance,'save_failure',side_effect=archive_or_save):
            report=execute(plan,str(candidate),artifacts,runtime=sys.executable,deadline_seconds=2)
        self.assertEqual(report['status'],'fail')
        self.assertFalse(owners[0].thread.is_alive())
        self.assertIsNotNone(owners[0].worker.process.returncode)
        history_failures=[failure for failure in report['failures'] if failure.get('history')=='reverse' and failure.get('error')=='oracle-state-leak']
        self.assertEqual(len(history_failures),1,report['failures'])
        if mode=='archive-error':
            self.assertNotIn('bundle',history_failures[0])
            self.assertTrue(report['history_spool']['retained'])
            self.assertEqual(Path(report['history_spool']['path']).read_bytes(),original_spools[0])
            self.assertTrue(owners[0].spool.file.closed)
            return report
        self.assertFalse(list(artifacts.glob('oracle-history-*.gz')))
        bundle=json.loads((Path(history_failures[0]['bundle'])/'original.json').read_text())
        self.assertEqual(bundle['candidate_kind'],'oracle-history')
        self.assertEqual(bundle['oracle_history'],{'schema':1,'roles':{'oracle':'reverse','candidate':'forward'},'requests':{'oracle':[request_for(item) for item in reversed(items[1:])],'candidate':[request_for(item) for item in items[:2]]}})
        details=bundle['diff'][0]
        self.assertEqual([row['id'] for row in details['prior_histories']['forward']],['bad'])
        self.assertEqual([row['id'] for row in details['prior_histories']['reverse']],['c','b'])
        self.assertEqual(details['prior_histories']['forward'][0]['input_sha256'],items[0]['input_sha256'])
        for name,answer in (('reverse_oracle',bundle['oracle']),('forward_oracle',bundle['candidate'])):
            raw=base64.b64decode(bundle['raw_replies_base64'][name])
            self.assertTrue(raw.startswith(b'  {'));self.assertTrue(raw.endswith(b'}   '))
            self.assertEqual(json.loads(raw),answer)
        self.assertEqual(bundle['oracle']['revisions'],{'main.bend':2})
        self.assertEqual(bundle['candidate']['revisions'],{'main.bend':1})
        return report

    def test_completed_reverse_divergence_survives_each_forward_failure_boundary(self):
        for mode,error in (
            ('mismatch',None),
            ('candidate-error','not-semantic-reply:oracle-error'),
            ('malformed','truncated-reply'),
            ('manifest','oracle-witness-source-missing'),
            ('native-error','oracle-error:original-forward-native-error'),
            ('budget','history-spool-artifact-limit'),
            ('late-forward','proof-context-isolation-pair-missing'),
        ):
            with self.subTest(mode=mode):
                report=self.exercise_failure(mode)
                forward=[failure for failure in report['failures'] if failure.get('history')!='reverse']
                if error is None:
                    self.assertEqual(forward[0]['class'],'semantic-mismatch')
                else:self.assertEqual(forward[0]['error'],error)
                original=json.loads((Path(forward[0]['bundle'])/'original.json').read_text())
                if mode=='malformed':self.assertEqual(base64.b64decode(original['raw_replies_base64']['candidate']),b'{candidate-original')
                if mode=='budget':self.assertEqual(json.loads(base64.b64decode(original['raw_replies_base64']['oracle']))['id'],'a')

    def test_archive_write_failure_keeps_the_original_bounded_spool(self):
        report=self.exercise_failure('archive-error')
        self.assertEqual(report['failures'][0]['error'],'not-semantic-reply:oracle-error')
        self.assertTrue((Path(report['failures'][0]['bundle'])/'original.json').is_file())
        evidence_failure=next(failure for failure in report['failures'] if failure['class']=='evidence-failure')
        self.assertEqual(evidence_failure['error'],'original-history-archive-write-error')


class RawHistorySpoolBoundaryTests(unittest.TestCase):
    def test_lazy_answers_preserve_full_payload_without_shared_decoded_state(self):
        with tempfile.TemporaryDirectory() as directory,closing(RawHistorySpool(directory)) as spool:
            specimen=ComparatorTests();specimen.setUp()
            answer=specimen.answer
            base={'file':'@Base.bend','start':100,'end':112}
            answer['base_entities']=[{'site':base,'name':'Base.law','kind':'law','target':base,'signatureOwner':base,'bodyOwner':None}]
            raw=b'  '+canonical(answer)+b'   '
            item={'id':'one','input_sha256':'input'}
            spool.append('forward',item,raw,digest(canonical(normalized(answer))))
            spool.append('reverse',item,raw,digest(canonical(normalized(answer))))
            answers=SpoolAnswers(spool,'forward')
            self.assertEqual(list(answers),['one']);self.assertEqual(len(answers),1)
            loaded=answers['one'];self.assertEqual(loaded,answer)
            loaded['base_entities'][0]['bodyOwner']=base.copy()
            loaded['revisions']['main.bend']=99;loaded['occurrences'].clear()
            self.assertEqual(answers['one'],answer)
            self.assertEqual(spool.load('forward','one'),raw)
            self.assertEqual(spool.load('reverse','one'),raw)
            with self.assertRaises(KeyError):answers['missing']

    def test_lazy_answers_reject_corrupt_authenticated_history(self):
        with tempfile.TemporaryDirectory() as directory,closing(RawHistorySpool(directory)) as spool:
            item={'id':'one','input_sha256':'input'}
            spool.append('forward',item,b'{"outcome":"valid"}','semantic')
            offset,length,size,checksum=spool.index[('forward','one')]
            spool.index[('forward','one')]=(offset,length,size,'0'*len(checksum))
            with self.assertRaisesRegex(ToolFailure,'corrupt-history-spool'):
                SpoolAnswers(spool,'forward')['one']

    def test_visible_spool_is_counted_and_removed_without_exceeding_byte_limit(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            spool=RawHistorySpool(directory)
            self.addCleanup(spool.close)
            self.assertEqual(list(Path(directory).iterdir()),[spool.path])
            item={'id':'one','input_sha256':'input'}
            with patch.dict('semantic_conformance.GATE_PROFILE',{'artifact_bytes':1}):
                with self.assertRaisesRegex(ToolFailure,'artifact-limit'):
                    spool.append('forward',item,b'original bytes','semantic')
            self.assertEqual(spool.path.stat().st_size,0)
            spool.close()
            self.assertEqual(list(Path(directory).iterdir()),[])

    def test_spool_creation_cannot_bypass_file_count_limit(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory,patch.dict('semantic_conformance.GATE_PROFILE',{'artifact_files':0}):
            with self.assertRaisesRegex(ToolFailure,'artifact-limit'):RawHistorySpool(directory)
            self.assertEqual(list(Path(directory).iterdir()),[])


if __name__=='__main__':unittest.main()
