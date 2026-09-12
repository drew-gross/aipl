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
