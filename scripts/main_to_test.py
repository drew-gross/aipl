"""Convert a case whose `fn main() !prints` only prints, pinned by `--- stdout ---`, into
`fn scenario() !prints` with a `.test` that captures the prints with a shim and asserts the old
stdout text. Usage: python3 scripts/main_to_test.py FILE.aipl ...  then `aipl check --fix` the files.
Prints `ok FILE` or the reason it skipped one (those are converted by hand). See CHECK_AS_GATE.md, Phase 2b."""
import re,sys
CAPTURE='''.test({
    mut out = "";
    shim prints {
        print = |text| {
            set out = `{out}{text}\\n`;
        },
        put = |text| {
            set out = `{out}{text}`;
        },
    } {
        scenario();
    };
    assert(out == EXPECTED);
})'''
def convert(path):
    s=open(path).read()
    m=re.search(r'^--- stdout ---\n',s,re.M)
    if not m: return 'no stdout section'
    body_text=s[m.end():]
    if re.search(r'^--- ',body_text,re.M): return 'more sections'
    head=s[:m.start()]
    mm=re.search(r'^(pub )?fn main\(\)( !\w+)*( -> i64)? \{\n',head,re.M)
    if not mm: return 'main shape'
    if 'fn scenario' in head: return 'scenario exists'
    effects=re.findall(r'!(\w+)',mm.group(0))
    if set(effects)-{'prints'}: return 'effects '+','.join(effects)
    if not mm.group(0).count('!prints') and 'print(' in head: return 'no !prints?'
    # find fn end: first "\n}\n" at column 0 after mm
    end=head.find('\n}\n',mm.end()-1)
    if end<0: return 'no end'
    fn_body=head[mm.end():end+1]
    # drop trailing literal 0 if main returned i64
    if mm.group(3):
        fb=fn_body.rstrip('\n')
        if fb.endswith('\n    0'): fn_body=fb[:-len('\n    0')]+'\n'
        else: return 'returns a value'
    rest=head[end+3:]  # after "\n}\n"
    if re.search(r'^(pub )?fn main\b',rest,re.M): return 'second main'
    expected_body=body_text.rstrip('\n')
    exp=expected_body+'\n' if expected_body!='' or 'print(' in head else ''
    if '\n' not in exp.rstrip('\n') :
        lit='"'+exp.replace('\\','\\\\').replace('"','\\"').replace('\n','\\n')+'"'
        if '{' in lit: lit=lit.replace('{','\\{')
    else:
        if '"""' in exp: return 'triple quote in output'
        ind=' '*8
        lines=exp.split('\n')  # last is '' => blank line before closing
        lit='"""\n'+'\n'.join((ind+l if l else '') for l in lines[:-1])+'\n\n'+ind+'"""'
    new_fn='fn scenario() !prints {\n'+fn_body+'}'+CAPTURE.replace('EXPECTED',lit)+'\n'
    out=head[:mm.start()]+new_fn+rest
    out=out.rstrip('\n')+'\n'
    # equality operator import
    if not re.search(r'equal as ==',out):
        im=re.search(r'^import \{([^}]*)\} from builtins;',out,re.M)
        if not im: return 'no builtins import'
        names=im.group(1).strip().rstrip(',')
        out=out[:im.start()]+'import { '+names+', equal as == } from builtins;'+out[im.end():]
    open(path,'w').write(out)
    return 'ok'
if __name__=='__main__':
    for p in sys.argv[1:]: print(convert(p),p)
