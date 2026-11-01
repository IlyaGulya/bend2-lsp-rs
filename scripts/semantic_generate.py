#!/usr/bin/env python3
"""Bounded typed-template projects, deliberately independent of either resolver."""
import hashlib
import json
import random
import re
from semantic_shapes import add_structure, shape_deck, source_depth

VERSION = 4
FAMILIES = ('lambda','let','pattern','prefix','dotted','offload','law','cross-law','base-inherited','ordered-diamond','proof-context','multiline','unicode-crlf','type-binders','constructor','do','template','parallel','law-binders')
PROFILE = {'version':VERSION,'projects':256,'max_files':4,'max_declarations':16,'max_file_bytes':16384,'max_project_bytes':65536,'max_depth':8,'max_roots':3,'families':list(FAMILIES),'entry_views':{family:2 if family=='cross-law' else 3 if family=='proof-context' else 1 for family in FAMILIES},'structural_shapes':602,'depth_metric':'typed construction scopes plus measured source delimiter/body/clause depth'}


def canonical(value):
    return json.dumps(value,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()


def _templates(seed=5301,projects=256):
    if projects not in (0,128,256,512):raise ValueError('unsupported reviewed calibration size')
    rng=random.Random(seed)
    for index in range(projects):
        family=FAMILIES[index % len(FAMILIES)]
        suffix=f'{index}_{rng.randrange(1 << 30)}'
        name='value'+suffix
        header='import Base\n\n'
        identity='def target(x: U32) -> U32:\n  x\n\n'
        files={}; roots=['main.bend']
        if family=='lambda':
            body='def wrap() -> (U32 -> U32) -> U32:\n  target => target(1)\n'
        elif family=='let':
            body='def other(x: U32) -> U32:\n  x\n\ndef main() -> U32:\n  target = other\n  target(1)\n'
        elif family=='pattern':
            body='def main(pair: (U32 -> U32) & U32) -> U32:\n  (target, value) = pair\n  target(value)\n'
        elif family in ('prefix','dotted'):
            files['dep.bend']=header+identity+'def Foo.target(x: U32) -> U32:\n  x\n'
            header+='import ./dep.bend as D\n\n'
            body='def main(D: U32) -> U32:\n  '+('D.target' if family=='prefix' else 'D.Foo.target')+'(1)\n'
        elif family=='offload':
            body='def main() -> U32:\n  target!(1)\n'
        elif family=='law':
            body='law identity:\n  for x: U32\n  U32\n\ndef identity(x):\n  target(x)\n\ndef main() -> U32:\n  identity(1)\n'
        elif family in ('cross-law','proof-context'):
            files['law.bend']=header+'law identity:\n  for x: U32\n  U32\n'
            roots=['a.bend','b.bend','c.bend'] if family=='proof-context' else ['a.bend','b.bend']
            for root,value in zip(roots,(1,2,3)):
                files[root]=header+'import ./law.bend as L\n\ndef L.identity(x):\n  '+('x' if value==1 else 'U32.add(x, 0)')+'\n\ndef main() -> U32:\n  L.identity(1)\n'
            body=''
        elif family in ('base-inherited','ordered-diamond'):
            files['dep.bend']=identity
            header+='import ./dep.bend as D\n'
            if family=='ordered-diamond':
                files['left.bend']='import ./dep.bend as Shared\n\ndef left(x: U32) -> U32:\n  Shared.target(x)\n'
                files['right.bend']='import ./dep.bend as Shared\n\ndef right(x: U32) -> U32:\n  Shared.target(x)\n'
                header+='import ./left.bend as L\nimport ./right.bend as R\n'
                body='def main() -> U32:\n  L.left(R.right(1))\n'
            else:
                body='def main() -> U32:\n  D.target(1)\n'
        elif family=='multiline':
            body='# Chooses the first value.\n# A second documentation line.\ndef choose(\n  x: U32,\n  y: U32\n) -> U32:\n  x\n\ndef main() -> U32:\n  choose(1, 2)\n'
        elif family=='unicode-crlf':
            body='# Origins after astral Unicode: λ 🦀\ndef main() -> U32:\n  target(1)\n'
        elif family=='type-binders':
            body='def identity(-A: Type, x: A) -> A:\n  x\n\ndef main() -> U32:\n  identity(U32, 1)\n'
        elif family=='constructor':
            body='type Box is Data:\n  Boxed{value: U32}\n\ndef main(box: Box) -> U32:\n  match box:\n    case Boxed{target}:\n      target\n'
        elif family=='template':
            body='def identity(~A: Type, x: A) -> A:\n  x\n\ndef main() -> U32:\n  identity(~U32, 1)\n'
        elif family=='parallel':
            body='def main(x: U32, y: U32) -> U32:\n  x y = x y\n  x\n'
        elif family=='law-binders':
            body='law witness:\n  for x: U32\n  exs y: U32\n  {x == y: U32}\n\ndef witness(x):\n  (x, {==})\n\nlaw constrained:\n  for x: U32 where {x == x: U32}\n  U32\n\ndef constrained(x):\n  (value, evidence) = x\n  value\n'
        else:
            body='def M.pure(x: U32) -> U32:\n  x\n\ndef M.bind(-A: Type, v: A, f: A -> U32) -> U32:\n  f(v)\n\ndef main() -> U32:\n  do M<>:\n    target: U32 <- M.pure(1)\n    return target\n'
        if not files or 'main.bend' in roots:
            files['main.bend']=header+identity+body
        # Capture-avoiding rename of a fresh, whole identifier. Definitions remain ordered.
        files={p:re.sub(r'\bvalue\b',name,t) for p,t in files.items()}
        if family=='unicode-crlf':files={p:t.replace('\n','\r\n') for p,t in files.items()}
        project={'schema':1,'id':f'generated-{index:03d}-{family}','facet':'valid','families':[family], 'seed':seed,'generator_version':VERSION,'roots':roots,'files':files,'revisions':{p:1 for p in files},'depth':source_depth(files),'metamorphic':'layout'}
        validate_bounds(project)
        yield project


def generate(seed=5301,projects=256):
    rng=random.Random(seed ^ 0x53454D414E544943)
    shapes=shape_deck(rng)
    if len(shapes)!=PROFILE['structural_shapes']:raise ValueError('structural profile shape count changed')
    if projects not in (0,128,256,512) or projects>len(shapes):raise ValueError('unsupported reviewed calibration size')
    inputs=set();structures=set()
    for index,case in enumerate(_templates(seed,projects)):
        shape=shapes[index]
        case=add_structure(case,shape,index,rng)
        identity=hashlib.sha256(canonical({'roots':case['roots'],'files':case['files']})).hexdigest()
        structure=canonical(shape)
        if identity in inputs or structure in structures:raise ValueError('duplicate generated project or construction structure')
        inputs.add(identity);structures.add(structure)
        if len(case['roots'])!=PROFILE['entry_views'][case['families'][0]]:raise ValueError('entry-view profile count changed')
        case['input_sha256']=identity
        validate_bounds(case)
        yield case


def validate_bounds(project):
    if not 1<=len(project['files'])<=PROFILE['max_files'] or not 1<=len(project['roots'])<=PROFILE['max_roots']:raise ValueError('project shape out of bounds')
    sizes=[len(text.encode()) for text in project['files'].values()]
    if max(sizes)>PROFILE['max_file_bytes'] or sum(sizes)>PROFILE['max_project_bytes'] or project.get('depth',0)>PROFILE['max_depth']:raise ValueError('project resources out of bounds')
    if project.get('resources',{}).get('declarations',0)>PROFILE['max_declarations']:raise ValueError('declaration resource bound')


def layout_variant(project):
    """Capture-avoiding fresh binder/alias rename plus explicit UTF8 layout mapping."""
    variant=json.loads(json.dumps(project)); maps={}
    names={'x':'renamed_x'}
    for text in project['files'].values():
        for alias in re.findall(r'(?m)^\s*import\s+\S+\s+as\s+([A-Za-z_][A-Za-z0-9_]*)',text):names[alias]='Renamed_'+alias
    fresh='|'.join(re.escape(name) for name in names.values())
    if any(re.search(r'\b(?:'+fresh+r')\b',text) for text in project['files'].values()):raise ValueError('metamorphic fresh-name collision')
    alternatives=b'|'.join(re.escape(name.encode()) for name in sorted(names,key=len,reverse=True))
    pattern=re.compile(rb'\r?\n|(?<![A-Za-z0-9_.])(?:'+alternatives+rb')(?![A-Za-z0-9_])')
    for name,source in project['files'].items():
        original=source.encode(); result=bytearray('# metamorphic layout: λ 🦀\r\n'.encode());mapping={};pos=0
        for match in pattern.finditer(original):
            unchanged=original[pos:match.start()]
            for offset in range(pos,match.start()+1):mapping[offset]=len(result)+(offset-pos)
            result.extend(unchanged)
            replacement=b'\r\n' if match[0] in (b'\n',b'\r\n') else names[match[0].decode()].encode()
            mapping[match.start()]=len(result);result.extend(replacement);mapping[match.end()]=len(result);pos=match.end()
        for offset in range(pos,len(original)+1):mapping[offset]=len(result)+(offset-pos)
        result.extend(original[pos:])
        variant['files'][name]=result.decode();maps[name]=mapping
        variant['revisions'][name]=project['revisions'].get(name,1)+1
    variant['id']+='-layout'
    variant['metamorphic']=['fresh-binder-rename','fresh-alias-rename','comments','CRLF']
    maps['@renames']=names
    variant['depth']=max(project.get('structural_shape',{}).get('construction_depth',1),source_depth(variant['files']))
    if 'resources' in variant:
        variant['resources']['source_bytes']=sum(len(text.encode()) for text in variant['files'].values())
        variant['resources']['structural_depth']=variant['depth']
    validate_bounds(variant)
    return variant,maps


def remap_answer(answer,maps):
    result=json.loads(json.dumps(answer))
    if 'revisions' in result:result['revisions']={name:revision+(name in maps) for name,revision in result['revisions'].items()}
    for field in ('declarations','occurrences'):
        for record in result[field]:
            if field=='declarations' and 'name' in record:
                head,separator,tail=record['name'].partition('.')
                record['name']=maps.get('@renames',{}).get(head,head)+separator+tail
            for key in ('site','target','signatureOwner','bodyOwner','enclosing'):
                site=record.get(key)
                if site and site['file'] in maps:
                    mapping=maps[site['file']]
                    site['start']=mapping[site['start']];site['end']=mapping[site['end']]
    return result
