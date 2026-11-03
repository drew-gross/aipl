"""Convert a case whose `fn main() !prints` only prints, pinned by `--- stdout ---`, into a
top-level `test({ .. })` block that runs the old body under a `shim prints` capturing the output
and asserts the old stdout text. Usage: python3 scripts/main_to_test.py FILE.aipl ... then
`aipl check --fix` the files. Prints `ok FILE` or the reason it skipped one (those are converted
by hand). See CHECK_AS_GATE.md, Phase 2b."""
import re,sys
def convert(path):
    s=open(path).read()
    m=re.search(r'^--- stdout ---\n',s,re.M)
    if not m: return 'no stdout section'
    body_text=s[m.end():]
    nxt=re.search(r'^--- ',body_text,re.M)
    tail=''
    if nxt:
        tail=body_text[nxt.start():]
        body_text=body_text[:nxt.start()]
        if re.search(r'^--- (cli|file|expect file)',tail,re.M): return 'cli/file section'
    head=s[:m.start()]
    mm=re.search(r'^(pub )?fn main\(\)( !\w+)*( -> i64)? \{\n',head,re.M)
    if not mm: return 'main shape'
    effects=re.findall(r'!(\w+)',mm.group(0))
    if set(effects)-{'prints'}: return 'effects '+','.join(effects)
    end=head.find('\n}\n',mm.end()-1)
    if end<0: return 'no end'
    fn_body=head[mm.end():end+1]
    if mm.group(3):
        fb=fn_body.rstrip('\n')
        if fb.endswith('\n    0'): fn_body=fb[:-len('\n    0')]+'\n'
        else: return 'returns a value'
    rest=head[end+3:]
    if re.search(r'^(pub )?fn main\b',rest,re.M): return 'second main'
    if re.search(r'^\s*return\b',fn_body,re.M): return 'return in body'
    expected_body=body_text.rstrip('\n')
    exp=expected_body+'\n' if expected_body!='' or 'print(' in head else ''
    if '\n' not in exp.rstrip('\n'):
        lit='"'+exp.replace('\\','\\\\').replace('"','\\"').replace('\n','\\n')+'"'
        lit=lit
    else:
        escaped=any(l!=l.rstrip() for l in exp.split('\n')) or '"""' in exp
        ind=' '*8
        lines=exp.split('\n')
        if escaped:
            # A `"""` block drops trailing whitespace and cannot hold `"""`.
            lit='"'+exp.replace('\\','\\\\').replace('"','\\"').replace('\n','\\n')+'"'
        else:
            lit='"""\n'+'\n'.join((ind+l if l else '') for l in lines[:-1])+'\n\n'+ind+'"""'
    body=''.join(('    '+l if l.strip() else l) for l in fn_body.splitlines(True))
    block=('test({\n    mut out = "";\n    shim prints {\n        print = |text| {\n'
           '            set out = `{out}{text}\\n`;\n        },\n        put = |text| {\n'
           '            set out = `{out}{text}`;\n        },\n    } {\n'+body+'    };\n'
           '    assert(out == '+lit+');\n})\n')
    out=head[:mm.start()]+block+rest
    out=out.rstrip('\n')+'\n'+tail
    if not re.search(r'equal as ==',out):
        im=re.search(r'^import \{([^}]*)\} from builtins;',out,re.M)
        if not im: return 'no builtins import'
        names=im.group(1).strip().rstrip(',')
        out=out[:im.start()]+'import { '+names+', equal as == } from builtins;'+out[im.end():]
    open(path,'w').write(out)
    return 'ok'
if __name__=='__main__':
    for p in sys.argv[1:]: print(convert(p),p)
