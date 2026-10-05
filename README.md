# AIPL

A small, statically typed, value-semantics language with a Cranelift backend.
The compiler is written in Rust and dogfoods the language: its own parser,
lexer, formatter and a growing share of its builtins are AIPL programs under
`crates/`, compiled into the compiler and called through the FFI.

This README is a stub. The language has no reference yet; the closest thing is
the generated documentation site under `docs/` (open `docs/index.html`), which
renders every `# ..` doc comment in the compiler's own AIPL sources, and the
`tests/cases/**/*.aipl` corpus, where each case is a real program alongside the
output it produces.

## Getting started

```
cargo build
./target/debug/aipl run   file.aipl          # compile and JIT-execute `main`
./target/debug/aipl build file.aipl -o app   # link a native executable
./target/debug/aipl check [path...]          # run every fn's `.test({ .. })` block
./target/debug/aipl fmt   file.aipl          # rewrite in canonical format (`--check` to report)
./target/debug/aipl docs  [path...] -o dir   # write the HTML documentation site
./target/debug/aipl lsp                      # serve the Language Server Protocol (editors spawn this)
```

`aipl check` is the test runner: a function carries its tests in a
`.test({ .. })` block after its body, and `check` runs them all, reports any
file that is not in canonical format, and exits non-zero on any failure.

A file imports what it uses, operators included, always aliased:

```
import { print, wrapping_add as + } from builtins;

fn main() !prints {
    print(`{1 + 2}`);
}
```

## Strings and escaping

There are four string literal forms. Two decode escapes, two are raw; two
interpolate, two do not.

| form | escapes | interpolation | dedent |
|---|---|---|---|
| `"..."` | `\n` `\t` `\r` `\\` `\"` | no | no |
| `"""..."""` | none — verbatim | no | yes |
| `` `...` `` | `\n` `\t` `\r` `\\` `` \` `` `\{` `\}` | `{expr}` | no |
| ```` ```...``` ```` | `\{` `\}` only | `{expr}` | yes |

(A char literal `'x'` takes the same escapes as `"..."` plus `\'`, and must hold
exactly one char.)

**Escapes.** In a form that decodes them, a backslash always claims the next
character: `\n`, `\t` and `\r` become the control characters, and any other
escapable character stands for itself. An escape outside the form's set is a
compile error (`unknown escape sequence \q`), not a literal backslash — so a
backslash in a `"..."` string is always written `\\`.

**Raw forms.** `"""..."""` decodes nothing: every backslash is content, and
there is no way to write `"""` inside one. ```` ```...``` ```` is verbatim too,
with one exception — the braces. `{` opens an interpolation in either template
form, so a literal brace needs an escape, and `\{` / `\}` are the escape *every*
template has, independent of the rest of its escape set. In a raw template a
backslash before any other character is content (```` ```C:\new``` ```` is
`C:\new`); before a brace it escapes it.

**Dedent.** Both triple forms are "raw and dedented": a blank line right after
the opener and a blank line right before the closer are dropped, and the common
leading-space indentation of the remaining lines is stripped. So

```
let json = """
    {"a": 1}
    """;
```

is `{"a": 1}` with no indentation and no surrounding newlines. Two consequences
to know: a single-line `""" x """` is `"x "` (the leading space is indent, the
trailing one is content), and a raw template dedents across its interpolations,
as one block. Use the triple forms for text that contains backslashes or quotes
— JSON, regular expressions, expected compiler output — and an ordinary escaped
literal when exact leading whitespace matters.

**Interpolation.** `{expr}` in a template inserts the value's rendering. A `str`
and a `char` insert bare (no quotes); everything else renders as `to_str`
would, so a `str` in an array still shows its quotes. Interpolations re-lex the
whole language, so they nest: `` `{`{x}`}` `` is fine, and a `{ .. }` inside the
expression — a set literal, a struct — balances against its own closing brace.
Templates are the idiomatic way to build strings; prefer them to `+++`.

Where this lives: the string rules are declared once in
`crates/aipl-codegen/src/lex_aipl.aipl` (escape sets, delimiters, dedent),
decoding in `unescape.aipl`, dedent in `process_raw_string.aipl`. The VS Code
grammar under `editors/` is generated from the same rule table.

## Editor support

`editors/vscode/` is a VS Code extension: highlighting from the generated
TextMate grammar, and everything else — go to definition (across files),
hover with the declaration's `# ..` docs, the outline, format-on-save, and
diagnostics — from `aipl lsp`, the language server built into the compiler
(`crates/aipl-lsp`). The editor spawns the `aipl` the project carries, so what
it reports is what `aipl check` reports. Any LSP-speaking editor can use the
same server.

## Pattern matching

`match` takes one or more scrutinees and tries its arms top to bottom:

```
fn rep_suffix(min: u64, max: u64?) -> str {
    match (min, max) {
        (1, some(1)) => "",
        (0, some(1)) => "?",
        (lo, some(hi)) => `\{{lo},{hi}\}`,
        (0, none) => "*",
        (1, none) => "+",
        (lo, none) => `\{{lo},\}`,
    }
}
```

Patterns nest: a tuple `(p, q)`, a constructor over patterns (`some(1)`,
`Rect(w, _)`, `ok((a, b))`), an integer/string/char literal, a binder, and `_`.
`Ctor(..)` matches a case without naming its payload. In a nested slot a bare
name binds the value unless it spells a case — `none`, or a capitalised name
(`Dot`) — which is how the checker tells `(lo, none)` apart. The whole match is
checked exhaustive: the last arm above is `(lo, none)`, not `_`, and that is
accepted because together with `(lo, some(hi))` it covers everything; a missing
shape is named in the error (`(_, none) is not matched`), an arm no value can
reach is refused, and a match on a literal column needs a `_` or binder arm.
`if (let (1, some(x)) = pair) { .. }` takes the same patterns.

## Debugging: `trace(expr)`

`trace(expr)` prints where it is, what was written there, and what it evaluated
to, then **evaluates to `expr`** — so it wraps a subexpression in place instead
of making you restructure the code around it:

```
fn area(w: i64, h: i64) -> i64 { trace(w) * trace(h) }
```

```
subdir/shapes.aipl:4 w = 3
subdir/shapes.aipl:4 h = 4
```

The file is named relative to the directory the compiler was invoked from, so a
trace locates itself in a tree rather than only in a file.

Two things make it convenient, and one makes that safe:

- **No import.** `trace` is ambient, like `assert`. Nothing else may be called
  that — a function, constant or imported name spelled `trace` is a compile
  error, because the call is rewritten before any name could shadow it.
- **No effect.** It is the only call that prints without one, so dropping a
  trace into a function never makes you restate its signature or its callers'.
- **`aipl check` refuses it.** A file that still calls `trace` — or that imports
  one that does — fails `check`, *after* its tests have run, so a trace a test
  hit has already printed by the time the diagnostic explains itself. That is
  what keeps the effect-free print from being a hole in the effect system.

Because it carries no effect, the optimizer is free to treat a trace as
removable: a call sunk into a branch that isn't taken does not print. That is
accepted — the point of `trace` is the ten minutes you spend with it, not
reliable logging.
