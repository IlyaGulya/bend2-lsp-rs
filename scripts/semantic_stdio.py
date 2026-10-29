#!/usr/bin/env python3
"""Selected real stdio consumers; bulk graph comparison remains exact and exhaustive."""
from pathlib import Path
import json
import os
import subprocess
import tempfile
import time
from urllib.parse import urlparse,unquote

import semantic_lsp
from semantic_lsp import LspProcess, MeasurementError
from semantic_conformance import ToolFailure, canonical
from semantic_install import SOURCE as COMPILER_SOURCE, digest, file_digest
import base64

BASE_FILE='@Base.bend'
SELECTED=('original-lambda','original-let_valid','original-pattern','original-import_prefix','original-import_dotted','original-dotted','original-gpu','original-law_pair','original-law_graph','original-metadata','cross-file-law-hierarchy','original-base','inherited-base')


def pinned_base_source():
    """Exact Base bytes provisioned by the production `bend base` contract."""
    pin=json.loads((COMPILER_SOURCE/'pin.json').read_text())
    source=(COMPILER_SOURCE/'base.bend').read_bytes()
    if pin['files']['base.bend']!=digest(source):raise ToolFailure('stdio-base-pin-mismatch')
    return source


def pinned_compiler(source,deadline):
    """The checksum-pinned genuine compiler must be provisioned outside measurement.

    Base is accepted only from that compiler's real `bend base` output, which
    must be byte-identical to the authenticated pin. No ambient compiler, Base
    library, adapter or network/install fallback is consulted here.
    """
    candidate=os.environ.get('BEND2_COMPILER_TEST_PATH')
    if not candidate:raise ToolFailure('stdio-pinned-compiler-missing')
    compiler=Path(candidate).resolve()
    if not compiler.is_file():raise ToolFailure('stdio-pinned-compiler-missing')
    pin=json.loads((COMPILER_SOURCE/'pin.json').read_text())
    def probe(argument):
        remaining=deadline-time.monotonic()
        if remaining<=0:raise ToolFailure('stdio-deadline')
        try:return subprocess.run([str(compiler),argument],stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,env={**os.environ,'BEND_NO_TELEMETRY':'1'},timeout=min(30,remaining),check=False)
        except (OSError,subprocess.TimeoutExpired) as error:raise ToolFailure('stdio-pinned-compiler-probe:'+type(error).__name__) from error
    version=probe('version');base=probe('base')
    if version.returncode!=0 or version.stdout.strip()!=(f"bend {pin['compiler_version']}").encode():raise ToolFailure('stdio-pinned-compiler-version-mismatch')
    if base.returncode!=0 or base.stdout!=source:raise ToolFailure('stdio-pinned-compiler-base-mismatch')
    return compiler


class ConsumerMismatch(ToolFailure):
    def __init__(self,category,item,expected,actual):
        super().__init__(category+':'+item['id'])
        self.category=category
        self.details={'expected':expected,'actual':actual}
        self.predicate=digest(canonical({'category':category,**self.details}))


class BaseDocument:
    """The live Base target document, admitted only as the exact pinned source."""
    def __init__(self,item,workspace,files,source):
        self.item=item
        self.workspace=workspace.resolve()
        self.fixtures={ (workspace/name).resolve() for name in files }
        self.source=source
        self.text=source.decode('utf-8')
        self.uri=None
    def site(self,uri,selection):
        if self.uri is None:
            parsed=urlparse(uri)
            if parsed.scheme!='file':raise ConsumerMismatch('stdio-base-target-scheme',self.item,'file',parsed.scheme)
            if parsed.netloc not in ('','localhost') or parsed.query or parsed.fragment:raise ConsumerMismatch('stdio-base-target-uri',self.item,'local unadorned file URI',uri)
            path=Path(unquote(parsed.path))
            if not path.is_absolute():raise ConsumerMismatch('stdio-base-target-uri',self.item,'absolute local file URI',uri)
            if path.name!='Base.bend':raise ConsumerMismatch('stdio-base-target-name',self.item,'Base.bend',path.name)
            if path.is_symlink() or not path.is_file():raise ConsumerMismatch('stdio-base-target-not-file',self.item,'regular Base.bend',str(path))
            resolved=path.resolve()
            if resolved==self.workspace or self.workspace in resolved.parents:raise ConsumerMismatch('stdio-base-target-inside-fixture',self.item,'compiler Base document',str(resolved))
            if resolved in self.fixtures:raise ConsumerMismatch('stdio-base-target-is-fixture',self.item,'compiler Base document',str(resolved))
            try:content=resolved.read_bytes()
            except OSError as error:raise ConsumerMismatch('stdio-base-target-unreadable',self.item,digest(self.source),str(error)) from error
            if content!=self.source:raise ConsumerMismatch('stdio-base-target-not-pinned-source',self.item,digest(self.source),digest(content))
            self.uri=uri
        elif uri!=self.uri:raise ConsumerMismatch('stdio-base-target-uri-changed',self.item,self.uri,uri)
        return {'file':BASE_FILE,'start':byte_offset(self.text,selection['start']),'end':byte_offset(self.text,selection['end'])}


class DeadlineLspProcess(LspProcess):
    def __init__(self,binary,workspace,deadline,compiler):
        self.deadline=deadline
        self.raw_messages=[];self.raw_bytes=0;self.requests=[]
        # The genuine compiler inherits this environment; its daily check must
        # never add network work inside the measured consumer phase.
        previous=os.environ.get('BEND_NO_TELEMETRY')
        os.environ['BEND_NO_TELEMETRY']='1'
        try:super().__init__(binary,workspace)
        finally:
            if previous is None:os.environ.pop('BEND_NO_TELEMETRY',None)
            else:os.environ['BEND_NO_TELEMETRY']=previous
        self.compiler_path=str(compiler)
        self.settings={'bend2-lsp':{'compilerPath':self.compiler_path,'compilerArguments':[]}}
    def _dispatch(self,body,received_ns):
        self.raw_bytes+=len(body)
        if self.raw_bytes>4_000_000:raise MeasurementError('semantic stdio transcript output limit')
        self.raw_messages.append(base64.b64encode(body).decode('ascii'))
        return super()._dispatch(body,received_ns)


    def request(self,method,params,notification=None):
        remaining=self.deadline-time.monotonic()
        if remaining<=0:raise ToolFailure('stdio-deadline')
        semantic_lsp.REQUEST_TIMEOUT=min(10,remaining)
        self.requests.append({'method':method,'params':params})
        return super().request(method,params,notification)


def position(text,offset):
    prefix=text.encode()[:offset].decode();line=prefix.count('\n');tail=prefix.rsplit('\n',1)[-1]
    return {'line':line,'character':len(tail.encode('utf-16-le'))//2}


def byte_offset(text,point):
    # Match the LF-only line boundaries used by compiler sites and LineIndex.
    lines=text.split('\n')
    line=point['line']
    line_count=len(lines)-(lines[-1]=='')
    if line>line_count:raise ToolFailure('stdio-position-outside-source')
    prefix='\n'.join(lines[:line])
    if 0<line<len(lines):prefix+='\n'
    tail=lines[line] if line<len(lines) else ''
    if line+1<len(lines):tail+='\n'
    raw=tail.encode('utf-16-le')[:point['character']*2]
    try:return len(prefix.encode())+len(raw.decode('utf-16-le').encode())
    except UnicodeError as error:raise ToolFailure('stdio-split-surrogate') from error


def locations(reply,workspace,files,base):
    if reply is None:return []
    if isinstance(reply,dict):reply=[reply]
    result=[]
    for item in reply:
        uri=item.get('uri',item.get('targetUri'));selection=item.get('range',item.get('targetSelectionRange'))
        if uri is None or selection is None:raise ToolFailure('stdio-malformed-location')
        try:relative=Path(unquote(urlparse(uri).path)).relative_to(workspace).as_posix()
        except ValueError:
            result.append(base.site(uri,selection));continue
        if relative not in files:raise ToolFailure('stdio-target-outside-fixture')
        result.append({'file':relative,'start':byte_offset(files[relative],selection['start']),'end':byte_offset(files[relative],selection['end'])})
    return sorted(result,key=repr)


def check_public_ranges(server,item,oracle,workspace,base):
    files=item['case']['files'];root_uri=(workspace/item['root']).as_uri();checked=0
    def location(value):
        return locations(value,workspace,files,base)[0] if value is not None else None
    for file in oracle['revisions']:
        document,_=server.request('bend2/semanticDocument',{'rootUri':root_uri,'textDocument':{'uri':(workspace/file).as_uri()}})
        if document.get('schemaVersion')!=1 or document.get('rootUri')!=root_uri or document.get('document',{}).get('version')!=item['case']['revisions'][file]:raise ToolFailure('stdio-stale-semantic-document')
        expected=[];actual=[]
        for occurrence in oracle['occurrences']:
            site=occurrence['site']
            if site['file']!=file:continue
            spelling=files[file].encode()[site['start']:site['end']].decode()
            editable=dict(site)
            if not (occurrence['role']=='binding' and occurrence['binding']=='local'):
                editable['start']=site['end']-len(spelling.rsplit('.',1)[-1].encode())
            status=occurrence.get('status','resolved' if occurrence.get('target') else 'unresolved')
            expected.append((site,editable,occurrence['role'],occurrence['enclosing'],occurrence.get('target'),status))
        for occurrence in document.get('occurrences',[]):
            full=occurrence['fullRange'];editable=occurrence['range']
            site={'file':file,'start':byte_offset(files[file],full['start']),'end':byte_offset(files[file],full['end'])}
            editing={'file':file,'start':byte_offset(files[file],editable['start']),'end':byte_offset(files[file],editable['end'])}
            role='type' if occurrence.get('inType') and occurrence['role'] in ('value','call') else occurrence['role']
            actual.append((site,editing,role,location(occurrence.get('enclosing')),location(occurrence.get('target')),occurrence.get('status')))
        if sorted(expected,key=repr)!=sorted(actual,key=repr):
            ranges_match=sorted([row[:4] for row in expected],key=repr)==sorted([row[:4] for row in actual],key=repr)
            category='stdio-source-or-editable-range-mismatch' if not ranges_match else 'stdio-occurrence-resolution-mismatch'
            raise ConsumerMismatch(category,item,sorted(expected,key=repr),sorted(actual,key=repr))
        expected_declarations=[(d['site'],d['kind'],d['name'],d['target'],d['signatureOwner'],d['bodyOwner']) for d in oracle['declarations'] if d['site']['file']==file]
        actual_declarations=[]
        for declaration in document.get('declarations',[]):
            name_range=declaration['nameRange'];site={'file':file,'start':byte_offset(files[file],name_range['start']),'end':byte_offset(files[file],name_range['end'])}
            actual_declarations.append((site,declaration['kind'],declaration['name'],location(declaration['target']),location(declaration['signatureOwner']),location(declaration['bodyOwner'])))
        if sorted(expected_declarations,key=repr)!=sorted(actual_declarations,key=repr):raise ConsumerMismatch('stdio-declaration-body-metadata-mismatch',item,sorted(expected_declarations,key=repr),sorted(actual_declarations,key=repr))
        if item['case'].get('consumer_profile',item['case_id'])=='original-metadata':
            choose=next(d for d in document['declarations'] if d['name']=='choose')
            def source_range(range_value):
                return files[file].encode()[byte_offset(files[file],range_value['start']):byte_offset(files[file],range_value['end'])].decode()
            expected_signature='\n'.join(('def choose(','  x: U32,','  y: U32',') -> U32'))
            expected_documentation='\n'.join(('# Returns its first argument.','# This is documentation for choose.'))
            if source_range(choose['signatureRange'])!=expected_signature:raise ConsumerMismatch('stdio-signature-range-convention',item,expected_signature,source_range(choose['signatureRange']))
            if source_range(choose['bodyRange'])!='x':raise ConsumerMismatch('stdio-body-range-convention',item,'x',source_range(choose['bodyRange']))
            if source_range(choose['documentationRange'])!=expected_documentation:raise ConsumerMismatch('stdio-documentation-range-convention',item,expected_documentation,source_range(choose['documentationRange']))
        checked+=len(expected)+len(expected_declarations)
    return checked


def check_hierarchy(server,item,oracle,workspace,base):
    """Project compiler entities to protocol owners without changing target identity."""
    files=item['case']['files']
    declarations={canonical(d['target']):d['target'] for d in oracle['declarations'] if d['kind'] in ('def','law')}
    def entity(target):
        records=[record for record in oracle['declarations'] if record['target']==target]
        canonical_record=next((record for record in records if record['site']==target),None)
        if canonical_record is None:raise ToolFailure('stdio-hierarchy-oracle-entity-missing')
        anchor=canonical_record['bodyOwner'] or canonical_record['signatureOwner']
        if any((record['bodyOwner'] or record['signatureOwner'])!=anchor for record in records):raise ToolFailure('stdio-hierarchy-oracle-owner-inconsistent')
        return canonical_record,anchor
    def selection(value):
        return locations([{'uri':value['uri'],'range':value['selectionRange']}],workspace,files,base)[0]
    def contains(extent,site):
        return extent['file']==site['file'] and extent['start']<=site['start']<=site['end']<=extent['end']
    def validate_item(value,target):
        declaration,expected=entity(target);actual=selection(value)
        if actual!=expected:raise ConsumerMismatch('stdio-hierarchy-body-owner-projection',item,expected,actual)
        if value['name']!=declaration['name']:raise ConsumerMismatch('stdio-hierarchy-logical-name',item,declaration['name'],value['name'])
        extent=locations([{'uri':value['uri'],'range':value['range']}],workspace,files,base)[0]
        if not contains(extent,actual):raise ConsumerMismatch('stdio-hierarchy-item-range-ownership',item,actual,extent)
        return extent
    def edited_site(occurrence):
        site=dict(occurrence['site']);spelling=files[site['file']].encode()[site['start']:site['end']].decode()
        site['start']=site['end']-len(spelling.rsplit('.',1)[-1].encode())
        return site
    calls=[occurrence for occurrence in oracle['occurrences'] if occurrence['role'] in ('call','offload') and occurrence['binding']=='global' and occurrence.get('target') and occurrence['target']['file']!='@Base.bend' and occurrence.get('enclosing')]
    checked=0
    for target in declarations.values():
        _,anchor=entity(target);uri=(workspace/anchor['file']).as_uri()
        prepared,_=server.request('textDocument/prepareCallHierarchy',{'textDocument':{'uri':uri},'position':position(files[anchor['file']],anchor['end']-1)})
        if not isinstance(prepared,list) or len(prepared)!=1:
            actual=[selection(value) for value in prepared] if isinstance(prepared,list) else prepared
            raise ConsumerMismatch('stdio-call-hierarchy-not-prepared',item,[anchor],actual)
        extent=validate_item(prepared[0],target)
        for direction in ('outgoing','incoming'):
            expected={}
            for call in calls:
                if (call['enclosing'] if direction=='outgoing' else call['target'])!=target:continue
                endpoint=call['target'] if direction=='outgoing' else call['enclosing'];key=canonical(endpoint)
                expected.setdefault(key,(endpoint,[]))[1].append(edited_site(call))
            expected_anchors=sorted((entity(endpoint)[1] for endpoint,_ in expected.values()),key=repr)
            edges,_=server.request('callHierarchy/'+direction+'Calls',{'item':prepared[0]})
            if not isinstance(edges,list):raise ToolFailure('stdio-malformed-hierarchy-edges')
            field='to' if direction=='outgoing' else 'from'
            actual_anchors=sorted((selection(edge[field]) for edge in edges),key=repr)
            if actual_anchors!=expected_anchors:raise ConsumerMismatch('stdio-hierarchy-'+direction+'-owner-projection',item,expected_anchors,actual_anchors)
            by_anchor={canonical(entity(endpoint)[1]):(endpoint,sites) for endpoint,sites in expected.values()}
            for edge in edges:
                endpoint,sites=by_anchor[canonical(selection(edge[field]))]
                endpoint_extent=validate_item(edge[field],endpoint)
                owner=prepared[0] if direction=='outgoing' else edge['from']
                owner_extent=extent if direction=='outgoing' else endpoint_extent
                actual=locations([{'uri':owner['uri'],'range':range_value} for range_value in edge['fromRanges']],workspace,files,base)
                wanted=sorted({canonical(site):site for site in sites}.values(),key=repr)
                if actual!=wanted:raise ConsumerMismatch('stdio-hierarchy-'+direction+'-source-ranges',item,wanted,actual)
                if any(not contains(owner_extent,site) for site in actual):raise ConsumerMismatch('stdio-hierarchy-call-range-ownership',item,owner_extent,actual)
            checked+=1
    if not checked:raise ToolFailure('stdio-empty-hierarchy-project')
    return checked



def check_local_rename(server,item,oracle,workspace,base):
    """Query each physical binder, including template binders copied into constructors."""
    files=item['case']['files'];checked=0
    targets={canonical(o['target']):o['target'] for o in oracle['occurrences'] if o['binding']=='local' and o.get('target')}
    for target in targets.values():
        uri=(workspace/target['file']).as_uri();point=position(files[target['file']],target['start'])
        uses=[o['site'] for o in oracle['occurrences'] if o.get('target')==target and o['role']!='binding']
        sites=sorted({canonical(site):site for site in [target]+uses}.values(),key=repr)
        for include_declaration in (False,True):
            reply,_=server.request('textDocument/references',{'textDocument':{'uri':uri},'position':point,'context':{'includeDeclaration':include_declaration}})
            expected=sites if include_declaration else sorted(uses,key=repr)
            actual=locations(reply,workspace,files,base)
            if actual!=expected:raise ConsumerMismatch('stdio-binder-reference-mismatch',item,expected,actual)
            checked+=1
        new_name='renamed_local'
        reply,_=server.request('textDocument/rename',{'textDocument':{'uri':uri},'position':point,'newName':new_name})
        if not isinstance(reply,dict):raise ConsumerMismatch('stdio-local-rename-missing',item,sites,reply)
        edits=[]
        for document_uri,changes in reply.get('changes',{}).items():
            edits.extend({'uri':document_uri,**change} for change in changes)
        for change in reply.get('documentChanges',[]):
            if 'textDocument' not in change:raise ToolFailure('stdio-unexpected-rename-resource-operation')
            edits.extend({'uri':change['textDocument']['uri'],**edit} for edit in change['edits'])
        actual=locations(edits,workspace,files,base)
        if actual!=sites or any(edit.get('newText')!=new_name for edit in edits):raise ConsumerMismatch('stdio-local-rename-mismatch',item,{'sites':sites,'newText':new_name},edits)
        checked+=1
    if not checked:raise ToolFailure('stdio-empty-local-rename-project')
    return checked


def run_stdio(plan,oracle_answers,binary,deadline,selected_ids=None):
    required=set(SELECTED if selected_ids is None else selected_ids)
    selected=[item for item in plan['planned'] if item['variant']=='original' and (item['case_id'] in required or selected_ids is None and item['case'].get('consumer_profile'))]
    if not required.issubset({item['case_id'] for item in selected}):raise ToolFailure('stdio-corpus-case-missing')
    report={'planned':len(selected),'completed':0,'passed':0,'checks':[]}
    source=pinned_base_source()
    compiler=pinned_compiler(source,deadline)
    report['compiler']={'sha256':file_digest(compiler),'base_sha256':digest(source)}
    return _run_selected(selected,oracle_answers,binary,deadline,source,compiler,report)


def _run_selected(selected,oracle_answers,binary,deadline,source,compiler,report):
    for item in selected:
        if time.monotonic()>=deadline:raise ToolFailure('stdio-deadline')
        semantic_lsp.REQUEST_TIMEOUT=max(.01,min(10,deadline-time.monotonic()))
        files=item['case']['files'];oracle=oracle_answers[item['id']]
        with tempfile.TemporaryDirectory(prefix='bend-semantic-stdio-') as temporary:
            workspace=Path(temporary);base=BaseDocument(item,workspace,files,source)
            for name,text in item['case'].get('disk_files',files).items():path=workspace/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(text,newline='')
            server=DeadlineLspProcess(Path(binary).resolve(),workspace,deadline,compiler)
            try:
                server.initialize()
                for name,text in files.items():server.notify('textDocument/didOpen',{'textDocument':{'uri':(workspace/name).as_uri(),'languageId':'bend','version':1,'text':text}})
                # The actual public endpoint must observe the current document generation.
                root_uri=(workspace/item['root']).as_uri()
                server.request('textDocument/documentSymbol',{'textDocument':{'uri':root_uri}})
                checked=check_public_ranges(server,item,oracle,workspace,base)
                for occurrence in oracle['occurrences']:
                    target=occurrence.get('target');site=occurrence['site']
                    if not target or occurrence['role']=='binding':continue
                    # Generic eligibility rule: a standard definition request has
                    # file/default-entry semantics, so Base is asserted only for
                    # the selected entry file, which shares the entry-rooted
                    # oracle context. Base targets at other sites still require an
                    # exact target+status match through the entry-rooted
                    # semantic document above; they are never compared against a
                    # different request context.
                    if target['file']==BASE_FILE and site['file']!=item['root']:continue
                    uri=(workspace/site['file']).as_uri();text=files[site['file']]
                    point=position(text,site['end']-1)
                    if occurrence['binding']=='local':
                        actual,_=server.request('textDocument/references',{'textDocument':{'uri':uri},'position':point,'context':{'includeDeclaration':False}})
                        expected=sorted((o['site'] for o in oracle['occurrences'] if o.get('target')==target and o['role']!='binding'),key=repr)
                        observed=locations(actual,workspace,files,base)
                        if observed!=expected:raise ConsumerMismatch('stdio-local-reference-mismatch',item,expected,observed)
                    else:
                        actual,_=server.request('textDocument/definition',{'textDocument':{'uri':uri},'position':point})
                        observed=locations(actual,workspace,files,base)
                        if observed!=[target]:raise ConsumerMismatch('stdio-global-definition-mismatch',item,[target],observed)
                    checked+=1
                if item['case'].get('consumer_profile') in ('upstream-dependent-pair','upstream-exists-where'):
                    checked+=check_local_rename(server,item,oracle,workspace,base)
                if checked==0:raise ToolFailure('stdio-empty-selected-consumer-case')
                if item['case'].get('consumer_profile',item['case_id'])=='original-metadata':
                    declaration=next(d for d in oracle['declarations'] if d['name']=='choose')
                    hover,_=server.request('textDocument/hover',{'textDocument':{'uri':root_uri},'position':position(files[item['root']],declaration['site']['start'])})
                    rendered=str(hover)
                    for expected in ('Returns its first argument.','This is documentation for choose.','x: U32','y: U32'):
                        if expected not in rendered:raise ConsumerMismatch('stdio-metadata-missing',item,expected,rendered)
                    if 'Documentation for main, not choose.' in rendered:raise ConsumerMismatch('stdio-body-documentation-leak',item,'choose documentation only',rendered)
                if item['case'].get('consumer_profile',item['case_id']) in ('original-gpu','original-law_graph','original-law_pair','cross-file-law-hierarchy'):
                    checked+=check_hierarchy(server,item,oracle,workspace,base)
                server.finish();report['completed']+=1;report['passed']+=1;report['checks'].append({'id':item['id'],'consumer_checks':checked})
            except (ToolFailure,MeasurementError,ValueError,KeyError,TypeError,IndexError) as error:
                failure=error if isinstance(error,ToolFailure) else ToolFailure('stdio-protocol-failure:'+str(error))
                failure.consumer_item=item;failure.consumer_oracle=oracle;failure.consumer_binary=str(Path(binary).resolve())
                failure.consumer_evidence={'category':getattr(failure,'category','stdio-protocol-failure'),'predicate':getattr(failure,'predicate',None),'details':getattr(failure,'details',None),'raw_messages_base64':server.raw_messages,'requests':server.requests,'stderr':server.stderr_tail(),'truncated':server.raw_bytes>4_000_000}
                raise failure
            finally:server.close()
    return report
