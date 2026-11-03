"""Take the `main` out of a compile-error fixture (`--- errors ---` only), without changing what it pins.

For each file: remove `fn main` if the `error:` lines are identical without it; otherwise rename it
`subject` (the broken code is there, so the function stays) if they are identical then; otherwise leave
the file alone and say so. The comparison is of the messages, never of the whole section, because a
refill pins whatever the compiler now says, including a different error. The section is then refilled
by `aipl check --fix` (positions move). Usage: python3 scripts/errors_fixture_main.py FILE.aipl ...
Prints `deleted|renamed|skipped FILE`."""
import os,re,subprocess,sys,shutil
sys.path.insert(0,os.path.dirname(__file__))
import lint_fixtures_to_tests as L
AIPL=os.path.abspath('target/debug/aipl')
def errors_of(text):
    m=re.search(r'^--- ',text,re.M)
    return re.findall(r'^error: .*$',text[m.start():],re.M) if m else []
def fixed(path,text):
    open(path,'w').write(text)
    subprocess.run([AIPL,'check','--fix',path],capture_output=True,text=True)
    return open(path).read()
def convert(path):
    original=open(path).read()
    m=re.search(r'^--- ',original,re.M)
    head,tail=original[:m.start()],original[m.start():]
    sections=set(re.findall(r'^--- ([a-z ]+?)(?::[^-]*)? ---\s*$',original,re.M))
    if sections!={'errors'}: return 'skipped (not errors-only)'
    mains=[it for it in L.split_items(head) if it[1]=='fn' and re.match(r'(pub )?fn main\b',it[2])]
    if not mains: return 'skipped (no main)'
    want=errors_of(original)
    deleted=head.replace(mains[0][2]+'\n','',1)
    candidates=[('deleted',deleted)]
    if not re.search(r'\bsubject\b',head):
        candidates.append(('renamed',re.sub(r'^(pub )?fn main\(','fn subject(',head,count=1,flags=re.M)))
    for how,new_head in candidates:
        after=fixed(path,new_head+tail)
        if errors_of(after)==want:
            return how
    open(path,'w').write(original)
    return 'skipped (errors change either way)'
if __name__=='__main__':
    for p in sys.argv[1:]: print(convert(p),p)
