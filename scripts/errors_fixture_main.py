"""Take the `main` out of a compile-error fixture (`--- errors ---` only), without changing what it pins.

For each file: remove `fn main` if the `error:` lines are identical without it; otherwise rename it
`subject` (the broken code is there, so the function stays) if they are identical then; otherwise leave
the file alone and say so. The comparison is of the messages, never of the whole section, because a
refill pins whatever the compiler now says, including a different error. The section is then refilled
by `aipl check --fix` (positions move). Usage: python3 scripts/errors_fixture_main.py FILE.aipl ...
Prints `deleted|renamed|skipped FILE`."""
import os,re,subprocess,sys,shutil,tempfile
sys.path.insert(0,os.path.dirname(__file__))
import lint_fixtures_to_tests as L
AIPL=os.path.abspath('target/debug/aipl')
def normalize(lines, renamed=False):
    """`error:` lines with what a rename or the rooting changes left out: the file path a loader error
    names, and `subject` read as `main`."""
    lines = [re.sub(r'^error: \S+\.aipl: ', 'error: <file>: ', l) for l in lines]
    return [l.replace('"subject"', '"main"') for l in lines] if renamed else lines


def pinned_errors(text):
    m = re.search(r'^--- ', text, re.M)
    return normalize(re.findall(r'^error: .*$', text[m.start():], re.M)) if m else []


def compile_errors(head, renamed):
    """The errors compiling `head` (a file with its sections cut off) really produces. Never read
    them back through `--fix`: that leaves a section alone when the file compiles, so it compares the
    old text with itself."""
    with tempfile.TemporaryDirectory() as dir:
        path = os.path.join(dir, 'probe.aipl')
        open(path, 'w').write(head)
        out = subprocess.run([AIPL, 'check', path], capture_output=True, text=True, cwd=dir)
    got = re.findall(r'^error: .*$', out.stdout + '\n' + out.stderr, re.M)
    return normalize(got, renamed)


def convert(path):
    original = open(path).read()
    m = re.search(r'^--- ', original, re.M)
    head, tail = original[:m.start()], original[m.start():]
    sections = set(re.findall(r'^--- ([a-z ]+?)(?::[^-]*)? ---\s*$', original, re.M))
    if sections != {'errors'}: return 'skipped (not errors-only)'
    mains = [it for it in L.split_items(head) if it[1] == 'fn' and re.match(r'(pub )?fn main\b', it[2])]
    if not mains: return 'skipped (no main)'
    want = pinned_errors(original)
    deleted = head.replace(mains[0][2] + '\n', '', 1)
    candidates = [('deleted', deleted)]
    if not re.search(r'\bsubject\b', head):
        candidates.append(('renamed', re.sub(r'^(pub )?fn main\(', 'fn subject(', head, count=1, flags=re.M)))
    for how, new_head in candidates:
        if compile_errors(new_head, how == 'renamed') == want:
            # Positions moved; `--fix` refills them from a compile that is known to fail.
            open(path, 'w').write(new_head + tail)
            subprocess.run([AIPL, 'check', '--fix', path], capture_output=True, text=True)
            return how
    return 'skipped (compiling it does not give the same errors either way)'


if __name__=='__main__':
    for p in sys.argv[1:]: print(convert(p),p)
