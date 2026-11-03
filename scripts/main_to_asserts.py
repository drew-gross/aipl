"""Convert a case whose `fn main() !prints` only prints, pinned by `--- stdout ---`, into a top-level
`test({ .. })` that asserts each printed value instead: `print(X);` becomes `assert(X == "<line>");`.
For cases that also carry a `--- report ---`, where a print-capturing shim (see main_to_test.py)
would add its own functions to the pinned table. Usage: python3 scripts/main_to_asserts.py FILE ...
then `aipl check --fix` the files (it also refills the report). Prints `ok FILE` or why it skipped."""
import re,sys
def lit(text):
    return '"'+text.replace('\\','\\\\').replace('"','\\"')+'"'
def convert(path):
    s=open(path).read()
    m=re.search(r'^--- stdout ---\n',s,re.M)
    if not m: return 'no stdout section'
    after=s[m.end():]
    nxt=re.search(r'^--- ',after,re.M)
    stdout=after[:nxt.start()] if nxt else after
    tail=after[nxt.start():] if nxt else ''
    if re.search(r'^--- (cli|file|expect file)',s,re.M): return 'cli/file section'
    head=s[:m.start()]
    mm=re.search(r'^(pub )?fn main\(\)( !\w+)*( -> i64)? \{\n',head,re.M)
    if not mm: return 'main shape'
    if set(re.findall(r'!(\w+)',mm.group(0)))-{'prints'}: return 'other effects'
    end=head.find('\n}\n',mm.end()-1)
    if end<0: return 'no end'
    body=head[mm.end():end+1]
    if mm.group(3):
        fb=body.rstrip('\n')
        if fb.endswith('\n    0'): body=fb[:-len('\n    0')]+'\n'
        else: return 'returns a value'
    rest=head[end+3:]
    if re.search(r'^(pub )?fn main\b',rest,re.M): return 'second main'
    lines=body.split('\n')
    prints=[i for i,l in enumerate(lines) if re.match(r'^\s*print\(',l)]
    if 'print(' in ''.join(l for i,l in enumerate(lines) if i not in prints): return 'print inside another construct'
    out_lines=stdout.rstrip('\n').split('\n') if stdout.strip('\n')!='' else []
    out_lines+=['']*(len(prints)-len(out_lines))  # trailing empty prints leave no lines
    if len(out_lines)!=len(prints): return 'prints and stdout lines differ'
    k=0
    for i in prints:
        l=lines[i]
        mt=re.match(r'^(\s*)print\((.*)\);\s*(//.*)?$',l)
        if not mt: return 'multi-line or odd print'
        lines[i]=f'{mt.group(1)}assert({mt.group(2)} == {lit(out_lines[k])});'+(' '+mt.group(3) if mt.group(3) else '')
        k+=1
    new_body='\n'.join(('    '+l if False else l) for l in lines)
    block='test({\n'+new_body.rstrip('\n')+'\n})\n'
    out=head[:mm.start()]+block+rest
    out=out.rstrip('\n')+'\n'+tail
    im=re.search(r'^import \{([^}]*)\} from builtins;',out,re.M)
    if not im: return 'no builtins import'
    names=[n.strip() for n in im.group(1).split(',') if n.strip()]
    if 'print' in names and 'print(' not in out.split('--- ')[0].replace('import','',1): names.remove('print')
    if 'equal as ==' not in names: names.append('equal as ==')
    out=out[:im.start()]+'import { '+', '.join(names)+' } from builtins;'+out[im.end():]
    open(path,'w').write(out)
    return 'ok'
if __name__=='__main__':
    for p in sys.argv[1:]: print(convert(p),p)
