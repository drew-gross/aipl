"""Move a lint's case fixtures into the `.test` block of the lint itself.

For a lint written in AIPL (`crates/aipl-codegen/src/lint/<L>.aipl`), the fixtures
`tests/cases/lints/{err_<L>,<L>_allow,<L>_not_flagged}.aipl` are replaced by assertions in the
lint's own tests, where the near-miss and the flagged shapes sit beside the code they test:

- `<L>_not_flagged`: one `assert(driver(<fn source>, L)? == [])` per function in the fixture.
- `err_<L>`: one assertion per function with the messages the lint gives it, read off by running
  the driver (so they are what the lint says, not hand-typed).
- `<L>_allow`: the functions become helpers declared in the test block and the printed lines
  become asserts (the `#[allow]` squelch and the rewrite checked side by side).

Usage (from the repo root): python3 scripts/lint_fixtures_to_tests.py LINT [LINT ...]
A lint whose conversion does not pass `aipl check` is restored and its fixtures kept; the reason is
printed. Fixtures are deleted only after the lint file passes. See CHECK_AS_GATE.md and the add-lint
skill ("Tests belong in the lint").
"""
import os
import re
import subprocess
import sys

LINTS = "crates/aipl-codegen/src/lint"
FIXTURES = "tests/cases/lints"
AIPL = "target/debug/aipl"


def strip_sections(text):
    m = re.search(r"^--- ", text, re.M)
    return text[: m.start()] if m else text


def scan_depth(line, depth, state):
    """Brace depth after `line`, skipping strings and comments. `state` carries a raw `\"\"\"`."""
    i = 0
    n = len(line)
    while i < n:
        c = line[i]
        if state["raw"]:
            if line.startswith('"""', i):
                state["raw"] = False
                i += 3
                continue
            i += 1
            continue
        if line.startswith('"""', i):
            state["raw"] = True
            i += 3
            continue
        if line.startswith("//", i):
            break
        if c == '"' or c == "`":
            q = c
            i += 1
            while i < n and line[i] != q:
                if line[i] == "\\":
                    i += 1
                i += 1
            i += 1
            continue
        if c == "'":
            # a char literal: 'x' or '\x'
            if i + 2 < n and line[i + 1] != "\\" and line[i + 2] == "'":
                i += 3
                continue
            if i + 3 < n and line[i + 1] == "\\" and line[i + 3] == "'":
                i += 4
                continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        i += 1
    return depth


def split_items(text):
    """[(comments, kind, source)] for the top-level items of a formatted fixture."""
    lines = strip_sections(text).split("\n")
    items = []
    pending = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.strip() == "":
            pending = []
            i += 1
            continue
        if line.startswith("//") or (line.startswith("#") and not line.startswith("#[")):
            pending.append(line)
            i += 1
            continue
        m = re.match(r"(pub )?(fn|struct|variant|let|import)\b", line)
        if not m:
            i += 1
            continue
        kind = m.group(2)
        start = i
        depth = 0
        state = {"raw": False}
        seen_open = False
        while i < len(lines):
            before = depth
            depth = scan_depth(lines[i], depth, state)
            if "{" in lines[i]:
                seen_open = True
            i += 1
            if kind in ("fn", "struct") and seen_open and depth == 0:
                break
            if kind in ("let", "import") and re.search(r";\s*(#\[allow\])?\s*$", lines[i - 1]):
                break
            if kind == "variant" and depth == 0 and not lines[i - 1].rstrip().endswith("="):
                nxt = lines[i] if i < len(lines) else ""
                if not nxt.startswith(" ") and not nxt.startswith("|"):
                    break
        items.append((pending, kind, "\n".join(lines[start:i])))
        pending = []
    return items


def comment_lines(comments, indent="    "):
    out = []
    for c in comments:
        c = c.rstrip()
        if c.startswith("//"):
            out.append(indent + c)
        else:
            out.append(indent + "//" + c[1:])
    return out


def raw_block(src, indent):
    body = "\n".join((indent + "    " + l) if l.strip() else "" for l in src.split("\n"))
    return '"""\n' + body + "\n" + indent + '"""'


def driver_of(lint_text, name):
    """The test driver the lint's own tests use, and what to apply to its result to get messages."""
    m = re.search(r"\b(lint_(?:function_)?(?:unfilled_)?messages|lint_messages)\(", lint_text)
    if m:
        return m.group(1), ""
    m = re.search(r"\b(lint_function_once_hits|lint_function_hits|lint_hits|lint_function_unfilled_hits)\(", lint_text)
    if m:
        return m.group(1), "$message"
    return None, ""


def run(cmd):
    return subprocess.run(cmd, capture_output=True, text=True)


def harvest(lint, driver, project, snippets):
    """The messages `driver(snippet, lint)` gives for each snippet, by running it: a list of
    lists of str, or None when the run did not print what was expected."""
    path = f"{LINTS}/__harvest_{lint}.aipl"
    text = open(f"{LINTS}/{lint}.aipl").read()
    calls = []
    for src in snippets:
        calls.append(
            f"    let r = {driver}({raw_block(src, '    ')}, {lint})?{project};\n"
            "    let n = r.len();\n    trace(n);\n    for (let m : r) {\n        trace(m);\n    }"
        )
    text += "\nfn harvest_all() {}.test({\n" + "\n".join(calls) + "\n})\n"
    text = add_builtins(text, ["len"])
    open(path, "w").write(text)
    try:
        out = run([AIPL, "check", path])
    finally:
        os.remove(path)
    records = []
    for line in (out.stdout + "\n" + out.stderr).split("\n"):
        m = re.match(r"^\S+:\d+ ([nm]) = (.*)$", line)
        if m:
            records.append([m.group(1), m.group(2)])
    result = []
    i = 0
    for _ in snippets:
        if i >= len(records) or records[i][0] != "n":
            return None
        count = int(records[i][1])
        i += 1
        msgs = []
        for _ in range(count):
            if i >= len(records) or records[i][0] != "m":
                return None
            msgs.append(records[i][1])
            i += 1
        result.append(msgs)
    return result


def aipl_str(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n") + '"'


def builtin_names(text):
    names = []
    for m in re.finditer(r"import \{([^}]*)\} from builtins;", text):
        names += [n.strip() for n in m.group(1).split(",") if n.strip()]
    return names


def add_builtins(lint_text, wanted):
    have = builtin_names(lint_text)
    missing = [w for w in wanted if w not in have]
    if not missing:
        return lint_text
    m = re.search(r"import \{([^}]*)\} from builtins;", lint_text)
    if not m:
        return "import { " + ", ".join(missing) + " } from builtins;\n" + lint_text
    names = [n.strip() for n in m.group(1).split(",") if n.strip()] + missing
    return lint_text[: m.start()] + "import { " + ", ".join(names) + " } from builtins;" + lint_text[m.end():]


def insert_in_test(lint_text, lint, block):
    m = re.search(rf"^pub fn {lint}\b", lint_text, re.M)
    if not m:
        return None
    t = lint_text.find("\n}.test({", m.start())
    if t < 0:
        return None
    close = lint_text.find("\n})\n", t)
    if close < 0:
        return None
    return lint_text[:close] + "\n" + block.rstrip("\n") + lint_text[close:]


def convert(lint, with_allow=True):
    lint_path = f"{LINTS}/{lint}.aipl"
    if not os.path.exists(lint_path):
        return "no lint file"
    original = open(lint_path).read()
    driver, project = driver_of(original, lint)
    if not driver:
        return "no usable driver in the lint's tests"
    files = {k: f"{FIXTURES}/{n}.aipl" for k, n in
             (("err", f"err_{lint}"), ("allow", f"{lint}_allow"), ("ok", f"{lint}_not_flagged"))}
    present = {k: p for k, p in files.items() if os.path.exists(p)}
    if not with_allow:
        present.pop("allow", None)
    if not present:
        return "no fixtures"
    block = []
    # not flagged
    if "ok" in present:
        block.append("    // Shapes the lint must leave alone.")
        for comments, kind, src in split_items(open(present["ok"]).read()):
            # A `#[allow]` is the pipeline's squelch, which the lint function never sees.
            if kind != "fn" or re.match(r"(pub )?fn main\b", src) or '"""' in src or "#[allow]" in src:
                continue
            block += comment_lines(comments)
            block.append(f"    assert({driver}({raw_block(src, '    ')}, {lint})?{project} == []);")
    # flagged
    if "err" in present:
        snippets = []
        metas = []
        for comments, kind, src in split_items(open(present["err"]).read()):
            if kind != "fn" or re.match(r"(pub )?fn main\b", src) or '"""' in src or "#[allow]" in src:
                continue
            snippets.append(src)
            metas.append(comments)
        values = harvest(lint, driver, project, snippets) if snippets else []
        if values is None:
            return "could not read the lint's messages"
        block.append("")
        block.append("    // Shapes the lint flags, and what it says of them.")
        for comments, src, value in zip(metas, snippets, values):
            block += comment_lines(comments)
            expected = "[" + ", ".join(aipl_str(m) for m in value) + "]"
            block.append(f"    assert({driver}({raw_block(src, '    ')}, {lint})?{project} == {expected});")
    # allow
    wanted = []
    if "allow" in present:
        text = open(present["allow"]).read()
        stdout_m = re.search(r"^--- stdout ---\n(.*)", text, re.M | re.S)
        items = split_items(text)
        mains = [s for _, k, s in items if k == "fn" and re.match(r"(pub )?fn main\b", s)]
        if not mains or not stdout_m:
            return "the allow fixture has no main/stdout to translate"
        body_lines = mains[0].split("\n")[1:-1]
        prints = [l for l in body_lines if re.match(r"^\s*print\((.*)\);\s*$", l)]
        lines = stdout_m.group(1).rstrip("\n").split("\n")
        if len(prints) != len(lines):
            return "the allow fixture's prints and stdout lines differ"
        block.append("")
        block.append("    // `#[allow]` keeps the longhand, and the rewrite it asks for agrees with it.")
        for comments, kind, src in items:
            if kind != "fn" or re.match(r"(pub )?fn main\b", src):
                continue
            block += comment_lines(comments)
            block += ["    " + l if l.strip() else l for l in src.split("\n")]
        expected = iter(lines)
        for l in body_lines:
            m = re.match(r"^(\s*)print\((.*)\);\s*(//.*)?$", l)
            if m:
                lit = aipl_str(next(expected))
                block.append(f"    assert({m.group(2)} == {lit});" + (" " + m.group(3) if m.group(3) else ""))
            else:
                block.append(l)
        for m in re.finditer(r"import \{([^}]*)\} from builtins;", text):
            wanted += [n.strip() for n in m.group(1).split(",") if n.strip() and n.strip() != "print"]
    new = insert_in_test(original, lint, "\n".join(block))
    if new is None:
        return "could not find the lint's .test block"
    new = add_builtins(new, wanted)
    open(lint_path, "w").write(new)
    fix = run([AIPL, "check", "--fix", lint_path])
    ok = fix.returncode == 0 and "0 failed" in fix.stdout and "error" not in fix.stderr
    if not ok:
        if os.environ.get("KEEP_FAILED"):
            open(os.environ["KEEP_FAILED"] + "/" + lint + ".failed.aipl", "w").write(open(lint_path).read())
        open(lint_path, "w").write(original)
        tail = (fix.stdout + fix.stderr).strip().split("\n")
        return "check failed after conversion: " + " | ".join(tail[:4])
    for p in present.values():
        os.remove(p)
    return "ok (%s)" % ", ".join(sorted(present))


def convert_with_fallback(lint):
    """The allow fixture is the part most likely not to translate (it may declare a struct or
    a variant, which a test block cannot hold); keep it and move the rest."""
    result = convert(lint)
    if result.startswith("ok"):
        return result
    again = convert(lint, with_allow=False)
    if again.startswith("ok"):
        return again + f"; the allow fixture stays ({result[:90]})"
    return result


if __name__ == "__main__":
    for lint in sys.argv[1:]:
        print(f"{lint}: {convert_with_fallback(lint)}")
