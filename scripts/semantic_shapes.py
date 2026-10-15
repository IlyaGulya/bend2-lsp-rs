"""Typed construction shapes for semantic search, not a parser or resolution oracle."""
from itertools import product
import re


def shape_deck(rng):
    """602 distinct bounded construction trees; round-robin keeps scope forms mandatory."""
    modes=[]
    for mode in range(6):
        options=[]
        for chain in ((1,2,3) if mode==1 else (1,)):
            for arity in ((2,3,4) if mode==3 else (2,)):
                for capture in ((False,True) if mode==2 else (False,)):
                    overhead=chain if mode==1 else 2 if mode==2 else 1 if mode in (3,4,5) else 0
                    for count in range(1,7-overhead):
                        for calls in product((0,1),repeat=count):
                            options.append({'mode':mode,'calls':list(calls),'chain':chain,'arity':arity,'capture':capture,'construction_depth':max(count+2+overhead,arity+2 if mode==3 else 1)})
        rng.shuffle(options);modes.append(options)
    result=[]
    while any(modes):
        for options in modes:
            if options:result.append(options.pop())
    return result


def source_depth(files):
    """Explicit delimiter/body/clause depth; construction metadata adds implicit let scopes."""
    deepest=1
    for source in files.values():
        delimiters=[];indents=[0];clauses=0;law=False
        for line in source.splitlines():
            if not line.strip() or line.lstrip().startswith('#'):continue
            indent=len(line)-len(line.lstrip())
            while indent<indents[-1]:indents.pop()
            if indent>indents[-1]:indents.append(indent)
            if indent==0:
                law=line.startswith('law ');clauses=0
            if law and re.match(r'\s*(for|exs)\s',line):clauses+=1
            deepest=max(deepest,len(indents)+len(delimiters)+clauses)
            for token in re.finditer(r'#[^\n]*|"(?:\\.|[^"\\])*"|\x27(?:\\.|[^\x27\\])*\x27|[(){}\[\]]',line):
                text=token[0]
                if text.startswith('#'):break
                if text in ('(','{','['):delimiters.append({'(':')','{':'}','[':']'}[text])
                elif text in (')','}',']') and delimiters and delimiters[-1]==text:delimiters.pop()
                deepest=max(deepest,len(indents)+len(delimiters)+clauses)
    return deepest


def add_structure(case,shape,index,rng):
    stem=f'{index}_{rng.randrange(1 << 30)}'
    global_name='target_'+stem
    alias='Import_'+stem
    files={name:re.sub(r'(?<![A-Za-z0-9_])target(?![A-Za-z0-9_])',global_name,text) for name,text in case['files'].items()}
    files={name:re.sub(r'(?<![A-Za-z0-9_.])D(?![A-Za-z0-9_])',alias,text) for name,text in files.items()}
    helper='helper_'+stem;apply='apply_'+stem;other='other_'+stem;function='scope_'+stem
    parameter=rng.choice(('x','input',global_name))
    mode=shape['mode']
    def expression(variable,callee=helper):
        value=variable
        for choice in shape['calls']:
            value=f'{callee}({value})' if choice==0 else f'U32.add({value}, {rng.randrange(32)})'
        return value
    prelude=f'\ndef {helper}(x: U32) -> U32:\n  x\n'
    if mode==0:
        stress=f'def {function}({parameter}: U32) -> U32:\n  {expression(parameter)}\n'
    elif mode==1:
        statements=''.join(f'  {parameter} = {expression(parameter)}\n' for _ in range(shape['chain']))
        stress=f'def {function}({parameter}: U32) -> U32:\n{statements}  {parameter}\n'
    elif mode==2:
        prelude+=f'\ndef {apply}(f: U32 -> U32, x: U32) -> U32:\n  f(x)\n'
        binder='inner_'+stem if shape['capture'] else parameter
        value=parameter if shape['capture'] else binder
        argument=str(rng.randrange(32)) if shape['capture'] else parameter
        stress=f'def {function}({parameter}: U32) -> U32:\n  {apply}(({binder} => {expression(value)}), {argument})\n'
    elif mode==3:
        fields=[parameter]+[f'field_{stem}_{number}' for number in range(1,shape['arity'])]
        typ=' & '.join('U32' for _ in fields)
        stress=f'def {function}(pair: {typ}) -> U32:\n  ({", ".join(fields)}) = pair\n  {expression(parameter)}\n'
    elif mode==4:
        stress=f'def {function}({parameter}: U32) -> U32:\n  {parameter} spare_{stem} = {expression(parameter)} {{{rng.randrange(32)}: U32}}\n  {parameter}\n'
    else:
        prelude+=f'\ndef {other}(x: U32) -> U32:\n  x\n'
        stress=f'def {function}({helper}: U32 -> U32, {parameter}: U32) -> U32:\n  {helper}({expression(parameter,other)})\n'
    for root in case['roots']:
        files[root]=files[root].rstrip()+'\n'+prelude+'\n'+stress
    graph_variant=rng.randrange(2)
    if case['families']==['ordered-diamond'] and graph_variant:
        files['main.bend']=re.sub(r'^'+re.escape('import ./dep.bend as '+alias)+r'\r?\n','',files['main.bend'],flags=re.MULTILINE)
        files['main.bend']=files['main.bend'].replace('L.left(R.right(1))','R.right(L.left(1))')
    if case['families']==['unicode-crlf']:
        files={name:text.replace('\r\n','\n').replace('\n','\r\n') for name,text in files.items()}
    case['files']=files
    case['structural_shape']={**shape,'import_graph_variant':graph_variant if case['families']==['ordered-diamond'] else 0}
    case['depth']=max(shape['construction_depth'],source_depth(files))
    case['resources']={'modules':len(files),'declarations':sum(len(re.findall(r'(?m)^(def|law|type)\s',text))+len(re.findall(r'(?m)^  [A-Za-z_][A-Za-z0-9_.]*\{',text)) for text in files.values()),'source_bytes':sum(len(text.encode()) for text in files.values()),'structural_depth':case['depth']}
    return case
