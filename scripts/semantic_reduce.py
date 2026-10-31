"""Structural shrink candidates; the actual compiler validates every repaired project."""
import copy
from pathlib import PurePosixPath
import posixpath
import re


def argument_parts(source,opening):
    """Return top-level argument productions, ignoring quoted strings and comments."""
    stack=[')'];parts=[];start=opening+1;position=start;quoted=None
    while position<len(source):
        char=source[position]
        if quoted:
            if char=='\\':position+=2;continue
            if char==quoted:quoted=None
        elif char in ('"',"'"):quoted=char
        elif char=='#':
            newline=source.find('\n',position)
            if newline<0:return None
            position=newline
        elif char in '({[':stack.append({'(':')','{':'}','[':']'}[char])
        elif char in ')}]':
            if char!=stack[-1]:return None
            stack.pop()
            if not stack:
                if source[start:position].strip():parts.append((start,position))
                return position,parts
        elif char==',' and len(stack)==1:parts.append((start,position));start=position+1
        position+=1
    return None


def remove_argument(source,opening,index):
    parsed=argument_parts(source,opening)
    if parsed is None:return None
    closing,parts=parsed
    if index>=len(parts):return None
    retained=[source[start:end].strip() for number,(start,end) in enumerate(parts) if number!=index]
    return opening+1,closing,', '.join(retained)


def structural_reductions(case):
    for name,source in sorted(case['files'].items()):
        for definition in re.finditer(r'(?m)^def\s+([A-Za-z_][A-Za-z0-9_.]*)\??\s*\(',source):
            atom=definition[1];opening=definition.end()-1;parsed=argument_parts(source,opening)
            if parsed is None:continue
            closing,parameters=parsed
            # Filling a law changes its telescope in another declaration: never guess it.
            if not source[closing+1:].lstrip().startswith('->'):continue
            for index in range(len(parameters)):
                edits={name:[remove_argument(source,opening,index)]};valid=True
                for consumer,text in sorted(case['files'].items()):
                    spellings=[atom] if consumer==name else []
                    for imported in re.finditer(r'(?m)^\s*import\s+(\S+)\s+as\s+([A-Za-z_][A-Za-z0-9_]*)',text):
                        target=posixpath.normpath((PurePosixPath(consumer).parent/imported[1]).as_posix())
                        if target==name:spellings.append(imported[2]+'.'+atom)
                    for spelling in spellings:
                        for call in re.finditer(r'(?<![A-Za-z0-9_.])'+re.escape(spelling)+r'\s*\(',text):
                            call_opening=call.end()-1
                            if consumer==name and call_opening==opening:continue
                            # A declaration of another entity is never a call repair.
                            if re.search(r'\bdef\s*$',text[max(0,call.start()-12):call.start()]):continue
                            repair=remove_argument(text,call_opening,index)
                            if repair is None:valid=False;break
                            edits.setdefault(consumer,[]).append(repair)
                        if not valid:break
                    if not valid:break
                if not valid:continue
                candidate=copy.deepcopy(case)
                for consumer,replacements in edits.items():
                    ordered=sorted(set(replacements),reverse=True)
                    if any(ordered[n][0]<ordered[n+1][1] for n in range(len(ordered)-1)):valid=False;break
                    text=candidate['files'][consumer]
                    for start,end,replacement in ordered:text=text[:start]+replacement+text[end:]
                    candidate['files'][consumer]=text
                if valid:yield 'remove-repaired-argument:'+name+':'+atom+':'+str(index),candidate
        lines=source.splitlines(keepends=True);offsets=[];at=0
        for line in lines:offsets.append(at);at+=len(line)
        for index,line in enumerate(lines):
            match=re.match(r'^(\s*)case\s',line)
            if match is None:continue
            indentation=len(match[1]);end=index+1
            while end<len(lines):
                text=lines[end]
                if text.strip() and len(text)-len(text.lstrip())<=indentation:break
                end+=1
            candidate=copy.deepcopy(case)
            candidate['files'][name]=source[:offsets[index]]+source[offsets[end] if end<len(lines) else len(source):]
            yield 'remove-case-branch:'+name+':'+str(index),candidate
        for wrapper in re.finditer(r'\{\s*([A-Za-z_][A-Za-z0-9_.]*|[0-9]+)\s*:\s*[A-Za-z_][A-Za-z0-9_.]*\s*\}',source):
            candidate=copy.deepcopy(case);candidate['files'][name]=source[:wrapper.start()]+wrapper[1]+source[wrapper.end():]
            yield 'remove-type-annotation:'+name,candidate
        for literal in re.finditer(r'(?<![A-Za-z0-9_.])[1-9][0-9]+(?![A-Za-z0-9_.])',source):
            candidate=copy.deepcopy(case);candidate['files'][name]=source[:literal.start()]+'0'+source[literal.end():]
            yield 'simplify-literal:'+name,candidate
