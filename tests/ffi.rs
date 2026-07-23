//! The embedding FFI: JIT-compile AIPL from a Rust host and call its functions.
//! (A Rust-only surface the `.aipl` cases framework can't exercise.)

use aipl::Engine;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn calls_a_scalar_function() {
    let e = Engine::compile(
        "import { wrapping_add as + } from builtins; pub fn add(a: i64, b: i64) -> i64 { a + b }",
    )
    .unwrap();
    assert_eq!(e.call("add", &[2, 3]).unwrap(), 5);
    assert_eq!(e.call("add", &[-10, 4]).unwrap(), -6);
}

#[test]
fn bool_and_char_marshal_as_i64() {
    let src = "\
import { %, == } from builtins;
pub fn is_even(n: i64) -> bool { n % 2 == 0 }
pub fn echo_char(c: char) -> char { c }";
    let e = Engine::compile(src).unwrap();
    assert_eq!(e.call("is_even", &[4]).unwrap(), 1); // true
    assert_eq!(e.call("is_even", &[7]).unwrap(), 0); // false
    assert_eq!(e.call("echo_char", &['Z' as i64]).unwrap(), 'Z' as i64);
}

#[test]
fn supports_higher_arity_than_the_cli_runner() {
    // The CLI's `run` path only wires up arity 0–2; the FFI goes further.
    let e = Engine::compile(
        "import { wrapping_add as + } from builtins; pub fn sum4(a: i64, b: i64, c: i64, d: i64) -> i64 { a + b + c + d }",
    )
    .unwrap();
    assert_eq!(e.call("sum4", &[1, 2, 3, 4]).unwrap(), 10);
}

#[test]
fn calls_reach_private_helpers_within_the_program() {
    // `pub` gates cross-file *imports*, not host FFI calls — the host compiled
    // the whole program, so it can call any function, and callees resolve.
    let src = "\
import { *, wrapping_add as + } from builtins;
fn helper(n: i64) -> i64 { n * 10 }
pub fn entry(n: i64) -> i64 { helper(n) + 1 }";
    let e = Engine::compile(src).unwrap();
    assert_eq!(e.call("entry", &[5]).unwrap(), 51);
    assert_eq!(e.call("helper", &[4]).unwrap(), 40);
}

#[test]
fn rejects_unknown_function() {
    let e = Engine::compile("pub fn one() -> i64 { 1 }").unwrap();
    assert!(e.call("missing", &[]).is_err());
}

#[test]
fn rejects_wrong_arity() {
    let e = Engine::compile("pub fn one() -> i64 { 1 }").unwrap();
    assert!(e.call("one", &[7]).is_err());
}

#[test]
fn rejects_non_scalar_parameter() {
    // The `i64`-only `call` can't marshal `str` — use `call_values` for that.
    let e = Engine::compile("pub fn id(s: str) -> str { s }").unwrap();
    assert!(e.call("id", &[0]).is_err());
}

#[test]
fn call_values_marshals_str_args_with_int_return() {
    // str args + i64 return — the shape the compiler will use for
    // `common_space_prefix`: a char-walk counting the shared leading spaces.
    let src = "\
import { wrapping_add as +, ==, && } from builtins;
fn go(a: str, b: str, i: i64) -> i64 {
    match (a[i]) {
        some(x) => match (b[i]) {
            some(y) => if (x == ' ' && y == ' ') { go(a, b, i + 1) } else { i },
            none => i
        },
        none => i
    }
}
pub fn common_space_prefix(a: str, b: str) -> i64 { go(a, b, 0) }";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::{Int, Str};
    // Inline (<= 7-byte) arguments.
    assert_eq!(
        e.call_values(
            "common_space_prefix",
            &[Str("    x".into()), Str("  y".into())]
        )
        .unwrap(),
        Int(2)
    );
    // Long (heap, > 7-byte) leading-space runs exercise the heap arg buffer.
    assert_eq!(
        e.call_values(
            "common_space_prefix",
            &[Str("          a".into()), Str("        b".into())]
        )
        .unwrap(),
        Int(8)
    );
    // A tab vs a space shares no leading-space prefix.
    assert_eq!(
        e.call_values(
            "common_space_prefix",
            &[Str(" a".into()), Str("\tb".into())]
        )
        .unwrap(),
        Int(0)
    );
}

#[test]
fn call_values_marshals_str_return() {
    // Identity returns one of the (borrowed) argument buffers; concat builds a
    // fresh heap string. Both must round-trip and free cleanly.
    let src = "\
import { +++ } from builtins;
pub fn id(s: str) -> str { s }
pub fn shout(s: str) -> str { s +++ \" is loud!\" }";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::Str;
    // Inline arg, inline return.
    assert_eq!(
        e.call_values("id", &[Str("hi".into())]).unwrap(),
        Str("hi".into())
    );
    // Heap arg; identity's return aliases that very buffer (copied out before free).
    assert_eq!(
        e.call_values("id", &[Str("a longer string".into())])
            .unwrap(),
        Str("a longer string".into())
    );
    // Freshly built heap return (> 7 bytes), released after the bytes are copied.
    assert_eq!(
        e.call_values("shout", &[Str("the alarm".into())]).unwrap(),
        Str("the alarm is loud!".into())
    );
    // Empty argument.
    assert_eq!(
        e.call_values("shout", &[Str("".into())]).unwrap(),
        Str(" is loud!".into())
    );
}

#[test]
fn call_values_marshals_optional_return() {
    // `T?` over a scalar/str core is returned through a hidden sret pointer and
    // marshaled back as `FfiValue::Opt`. (Bool params take `Int` 0/1.)
    let src = "\
pub fn maybe_int(present: bool) -> i64? { if (present) { some(42) } else { none } }
pub fn maybe_str(present: bool) -> str? { if (present) { some(\"a long present string\") } else { none } }
pub fn nested(outer: bool, inner: bool) -> str?? {
    if (outer) { if (inner) { some(some(\"deep\")) } else { some(none) } } else { none }
}";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::{Int, Opt, Str};
    let some = |v| Opt(Some(Box::new(v)));

    // i64?: some(value) / none.
    assert_eq!(
        e.call_values("maybe_int", &[Int(1)]).unwrap(),
        some(Int(42))
    );
    assert_eq!(e.call_values("maybe_int", &[Int(0)]).unwrap(), Opt(None));

    // str?: a present heap string is copied out (and its retained reference
    // released); absent is none.
    assert_eq!(
        e.call_values("maybe_str", &[Int(1)]).unwrap(),
        some(Str("a long present string".into()))
    );
    assert_eq!(e.call_values("maybe_str", &[Int(0)]).unwrap(), Opt(None));

    // str??: the flattened tag (0 / 1 / 2) reconstructs as nested Opts.
    assert_eq!(
        e.call_values("nested", &[Int(0), Int(0)]).unwrap(),
        Opt(None)
    );
    assert_eq!(
        e.call_values("nested", &[Int(1), Int(0)]).unwrap(),
        some(Opt(None))
    );
    assert_eq!(
        e.call_values("nested", &[Int(1), Int(1)]).unwrap(),
        some(some(Str("deep".into())))
    );
}

#[test]
fn call_values_marshals_struct_return() {
    // A struct of scalar/str fields is returned through a hidden sret pointer and
    // marshaled back as `FfiValue::Struct` — the shape the compiler uses for
    // `find_trailing_whitespace` to report a `Span`. Fields come back in
    // declaration order, each tagged with its name.
    let src = "\
import { +++ } from builtins;
struct Span { start: i64, end: i64 }
struct Tagged { name: str, ok: bool, code: char }
pub fn span(a: i64, b: i64) -> Span { Span { start: a, end: b } }
pub fn tagged(suffix: str, present: bool) -> Tagged {
    Tagged { name: \"item-\" +++ suffix, ok: present, code: 'Z' }
}";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::{Int, Str, Struct};

    // Two-i64 struct (Span): both fields ride the sret buffer back.
    assert_eq!(
        e.call_values("span", &[Int(3), Int(7)]).unwrap(),
        Struct(vec![("start".into(), Int(3)), ("end".into(), Int(7))])
    );

    // Mixed fields: a freshly-built heap `str` (copied out, its retained
    // reference released), a `bool` (Int 0/1), and a `char` (codepoint).
    assert_eq!(
        e.call_values("tagged", &[Str("longvalue".into()), Int(1)])
            .unwrap(),
        Struct(vec![
            ("name".into(), Str("item-longvalue".into())),
            ("ok".into(), Int(1)),
            ("code".into(), Int('Z' as i64)),
        ])
    );
    // `false` comes back as Int(0).
    assert_eq!(
        e.call_values("tagged", &[Str("x".into()), Int(0)]).unwrap(),
        Struct(vec![
            ("name".into(), Str("item-x".into())),
            ("ok".into(), Int(0)),
            ("code".into(), Int('Z' as i64)),
        ])
    );
}

#[test]
fn call_values_marshals_optional_struct_return() {
    // `Span?` — an optional whose core is a struct — rides the sret pointer as a
    // flattened `{ tag, Span }`, marshaled back as `Opt(Some(Struct))` / `Opt(None)`.
    // This is the shape `find_trailing_whitespace` uses (no sentinel value).
    let src = "\
struct Span { start: i64, end: i64 }
pub fn span(present: bool, a: i64, b: i64) -> Span? {
    if (present) { some(Span { start: a, end: b }) } else { none }
}";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::{Int, Opt, Struct};
    let some_span = |a, b| {
        Opt(Some(Box::new(Struct(vec![
            ("start".into(), Int(a)),
            ("end".into(), Int(b)),
        ]))))
    };
    assert_eq!(
        e.call_values("span", &[Int(1), Int(3), Int(7)]).unwrap(),
        some_span(3, 7)
    );
    assert_eq!(
        e.call_values("span", &[Int(0), Int(3), Int(7)]).unwrap(),
        Opt(None)
    );
}

#[test]
fn call_values_marshals_struct_param() {
    // A struct of scalar fields passed as `FfiValue::Struct` is written into a
    // caller-allocated buffer; the callee receives a pointer to it — the same
    // ABI used for struct locals and returns, but on the input side. This is
    // the shape `caret_block` uses for its `Span` parameter.
    let src = "\
import { wrapping_add as +, wrapping_sub as - } from builtins;
struct Span { start: i64, end: i64 }
pub fn span_len(span: Span) -> i64 { span.end - span.start }
pub fn span_sum(a: Span, b: Span) -> i64 { a.start + a.end + b.start + b.end }";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::{Int, Struct};
    let span = |start, end| Struct(vec![("start".into(), Int(start)), ("end".into(), Int(end))]);

    assert_eq!(e.call_values("span_len", &[span(3, 10)]).unwrap(), Int(7));
    assert_eq!(e.call_values("span_len", &[span(0, 0)]).unwrap(), Int(0));
    // Two struct params.
    assert_eq!(
        e.call_values("span_sum", &[span(1, 2), span(3, 4)])
            .unwrap(),
        Int(10)
    );
    // Wrong field name is rejected.
    let bad = Struct(vec![("begin".into(), Int(0)), ("end".into(), Int(5))]);
    assert!(e.call_values("span_len", &[bad]).is_err());
    // Wrong field count is rejected.
    let short = Struct(vec![("start".into(), Int(0))]);
    assert!(e.call_values("span_len", &[short]).is_err());
    // FfiValue::Struct for a non-struct param is rejected.
    let src2 = "pub fn id(x: i64) -> i64 { x }";
    let e2 = Engine::compile(src2).unwrap();
    assert!(e2.call_values("id", &[span(1, 2)]).is_err());
}

#[test]
fn call_values_marshals_nested_composite_struct() {
    // A struct field may itself be a composite — a nested struct, a variant, or an
    // array — read inline/recursively. This is the `Token<K>`-shaped case the AIPL
    // lexer needs: `{ kind: <variant>, span: <struct>, tags: <array> }`.
    let src = "\
struct Span { start: i64, end: i64 }
variant Kind = Word(str) | Punct
struct Tok { kind: Kind, span: Span, tags: i64[] }
pub fn tok(a: i64, b: i64) -> Tok {
    Tok { kind: Word(\"a heap identifier\"), span: Span { start: a, end: b }, tags: [7, 8] }
}
pub fn toks() -> Tok[] {
    [Tok { kind: Word(\"first\"), span: Span { start: 0, end: 5 }, tags: [] },
     Tok { kind: Punct, span: Span { start: 5, end: 6 }, tags: [1] }]
}";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::{Array, Int, Str, Struct, Variant};

    let span = |a, b| Struct(vec![("start".into(), Int(a)), ("end".into(), Int(b))]);
    let tok = |kind, a, b, tags| {
        Struct(vec![
            ("kind".into(), kind),
            ("span".into(), span(a, b)),
            ("tags".into(), Array(tags)),
        ])
    };

    // A single nested struct with a variant field (str payload), a struct field,
    // and an array field.
    assert_eq!(
        e.call_values("tok", &[Int(3), Int(9)]).unwrap(),
        tok(
            Variant("Word".into(), vec![Str("a heap identifier".into())]),
            3,
            9,
            vec![Int(7), Int(8)]
        )
    );

    // An array of such structs — the token-stream shape.
    assert_eq!(
        e.call_values("toks", &[]).unwrap(),
        Array(vec![
            tok(
                Variant("Word".into(), vec![Str("first".into())]),
                0,
                5,
                vec![]
            ),
            tok(Variant("Punct".into(), vec![]), 5, 6, vec![Int(1)]),
        ])
    );
}

#[test]
fn call_values_rejects_unmarshalable_return() {
    // A type the FFI still can't marshal (a set) is rejected with a clear error.
    let src = "pub fn make_set() -> #{i64} { #{1, 2, 3} }";
    let e = Engine::compile(src).unwrap();
    assert!(e.call_values("make_set", &[]).is_err());
}

/// A variant whose cases span a nullary case and scalar/`str`/`char` payloads —
/// the shape the AIPL lexer's token type (`AiplTok`) uses.
const VARIANT_SRC: &str = "\
import { ==, +++ } from builtins;
variant Token = Eof | Ident(str) | Count(i64) | Mark(char)
pub fn classify(n: i64) -> Token {
    if (n == 0) { Eof }
    else { if (n == 1) { Ident(\"a long ident value\" +++ \"!\") }
    else { if (n == 2) { Count(42) }
    else { Mark('Z') } } }
}
pub fn describe(t: Token) -> str {
    match (t) {
        Eof => \"eof\",
        Ident(s) => s,
        Count(n) => \"count\",
        Mark(c) => \"mark\",
    }
}
pub fn count_of(t: Token) -> i64 {
    match (t) {
        Count(n) => n,
        Eof => 0,
        Ident(s) => 0,
        Mark(c) => 0,
    }
}
pub fn maybe_tok(present: bool) -> Token? {
    if (present) { some(Count(7)) } else { none }
}";

#[test]
fn call_values_marshals_variant_return() {
    // A variant is returned through a hidden sret pointer and marshaled back as
    // `FfiValue::Variant(case_name, payload)` — the active case's constructor name
    // plus its payload values in positional order (empty for a nullary case).
    let e = Engine::compile(VARIANT_SRC).unwrap();
    use aipl::FfiValue::{Int, Str, Variant};

    // Nullary case: empty payload.
    assert_eq!(
        e.call_values("classify", &[Int(0)]).unwrap(),
        Variant("Eof".into(), vec![])
    );
    // `str` payload: a freshly-built heap string, copied out with its retained
    // reference released.
    assert_eq!(
        e.call_values("classify", &[Int(1)]).unwrap(),
        Variant("Ident".into(), vec![Str("a long ident value!".into())])
    );
    // Scalar (i64) payload.
    assert_eq!(
        e.call_values("classify", &[Int(2)]).unwrap(),
        Variant("Count".into(), vec![Int(42)])
    );
    // `char` payload rides its codepoint as an `Int`.
    assert_eq!(
        e.call_values("classify", &[Int(9)]).unwrap(),
        Variant("Mark".into(), vec![Int('Z' as i64)])
    );
}

#[test]
fn call_values_marshals_variant_param() {
    // A variant passed as `FfiValue::Variant` is written into a caller-allocated
    // buffer (tag at offset 0, payload at each field's offset); the callee gets a
    // pointer to it — the same ABI as a struct param, on the input side.
    let e = Engine::compile(VARIANT_SRC).unwrap();
    use aipl::FfiValue::{Int, Str, Variant};

    // Nullary case round-trips through `describe`.
    assert_eq!(
        e.call_values("describe", &[Variant("Eof".into(), vec![])])
            .unwrap(),
        Str("eof".into())
    );
    // A `str` payload (long enough to be heap, exercising the borrowed-str path).
    assert_eq!(
        e.call_values(
            "describe",
            &[Variant(
                "Ident".into(),
                vec![Str("the identifier text".into())]
            )]
        )
        .unwrap(),
        Str("the identifier text".into())
    );
    // A scalar payload survives the round trip: `count_of(Count(n)) == n`.
    assert_eq!(
        e.call_values("count_of", &[Variant("Count".into(), vec![Int(99)])])
            .unwrap(),
        Int(99)
    );

    // Unknown case name is rejected.
    assert!(e
        .call_values("describe", &[Variant("Nope".into(), vec![])])
        .is_err());
    // Wrong payload arity is rejected.
    assert!(e
        .call_values("describe", &[Variant("Ident".into(), vec![])])
        .is_err());
    // FfiValue::Variant for a non-variant param is rejected.
    let e2 = Engine::compile("pub fn id(x: i64) -> i64 { x }").unwrap();
    assert!(e2
        .call_values("id", &[Variant("Eof".into(), vec![])])
        .is_err());
}

#[test]
fn call_values_marshals_optional_variant_return() {
    // `Token?` — an optional whose core is a variant — rides the sret pointer as a
    // flattened `{ tag, Token }`, marshaled back as `Opt(Some(Variant))` / `Opt(None)`.
    let e = Engine::compile(VARIANT_SRC).unwrap();
    use aipl::FfiValue::{Int, Opt, Variant};
    assert_eq!(
        e.call_values("maybe_tok", &[Int(1)]).unwrap(),
        Opt(Some(Box::new(Variant("Count".into(), vec![Int(7)]))))
    );
    assert_eq!(e.call_values("maybe_tok", &[Int(0)]).unwrap(), Opt(None));
}

#[test]
fn call_values_marshals_variant_with_composite_payload() {
    // A variant payload may itself be a composite (here an array), read
    // recursively — the active case's payload comes back nested.
    let src = "\
variant Bag = Full(i64[]) | Empty
pub fn full() -> Bag { Full([1, 2]) }
pub fn empty() -> Bag { Empty }";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::{Array, Int, Variant};
    assert_eq!(
        e.call_values("full", &[]).unwrap(),
        Variant("Full".into(), vec![Array(vec![Int(1), Int(2)])])
    );
    assert_eq!(
        e.call_values("empty", &[]).unwrap(),
        Variant("Empty".into(), vec![])
    );
}

/// Array returns over a spread of element types: scalars, `str`, `bool`
/// (bit-packed), `char` (str-shaped), nested arrays, structs, and variants — the
/// last being the shape the AIPL lexer needs (a `Token[]` token stream).
const ARRAY_SRC: &str = "\
import { == } from builtins;
struct Span { start: i64, end: i64 }
variant Token = Eof | Ident(str) | Count(i64)
pub fn ints(n: i64) -> i64[] { if (n == 0) { [] } else { [10, 20, 30] } }
pub fn strs() -> str[] { [\"alpha\", \"beta is a longer word\", \"gamma\"] }
pub fn bools() -> bool[] { [true, false, true, true] }
pub fn chars() -> char[] { ['a', 'b', 'c'] }
pub fn nested() -> i64[][] { [[1, 2], [], [3]] }
pub fn spans() -> Span[] { [Span { start: 1, end: 2 }, Span { start: 3, end: 4 }] }
pub fn tokens() -> Token[] { [Eof, Ident(\"a heap identifier\"), Count(7)] }
pub fn maybe(present: bool) -> i64[]? { if (present) { some([1, 2]) } else { none } }";

#[test]
fn call_values_marshals_array_return() {
    let e = Engine::compile(ARRAY_SRC).unwrap();
    use aipl::FfiValue::{Array, Int, Str, Struct, Variant};

    // Scalars, plus the empty-array case (an empty `Array`).
    assert_eq!(
        e.call_values("ints", &[Int(1)]).unwrap(),
        Array(vec![Int(10), Int(20), Int(30)])
    );
    assert_eq!(e.call_values("ints", &[Int(0)]).unwrap(), Array(vec![]));

    // `str[]`: each element's bytes copied out (a mix of inline and heap strings);
    // the block's single reference releases them.
    assert_eq!(
        e.call_values("strs", &[]).unwrap(),
        Array(vec![
            Str("alpha".into()),
            Str("beta is a longer word".into()),
            Str("gamma".into()),
        ])
    );

    // `bool[]` is bit-packed; each bit comes back as Int 0/1.
    assert_eq!(
        e.call_values("bools", &[]).unwrap(),
        Array(vec![Int(1), Int(0), Int(1), Int(1)])
    );

    // `char[]` is str-shaped: decoded to codepoints.
    assert_eq!(
        e.call_values("chars", &[]).unwrap(),
        Array(vec![Int('a' as i64), Int('b' as i64), Int('c' as i64)])
    );

    // Nested arrays, including an empty inner array.
    assert_eq!(
        e.call_values("nested", &[]).unwrap(),
        Array(vec![
            Array(vec![Int(1), Int(2)]),
            Array(vec![]),
            Array(vec![Int(3)]),
        ])
    );

    // Array of structs: each element read inline, field by field.
    assert_eq!(
        e.call_values("spans", &[]).unwrap(),
        Array(vec![
            Struct(vec![("start".into(), Int(1)), ("end".into(), Int(2))]),
            Struct(vec![("start".into(), Int(3)), ("end".into(), Int(4))]),
        ])
    );

    // Array of variants — the lexer's token-stream shape. The `str` payload is
    // borrowed from the block (copied out, released with the block).
    assert_eq!(
        e.call_values("tokens", &[]).unwrap(),
        Array(vec![
            Variant("Eof".into(), vec![]),
            Variant("Ident".into(), vec![Str("a heap identifier".into())]),
            Variant("Count".into(), vec![Int(7)]),
        ])
    );
}

#[test]
fn call_values_marshals_optional_array_return() {
    // `i64[]?` — an optional whose core is an array — rides the sret pointer, its
    // present core array read (and released) like a bare array return.
    let e = Engine::compile(ARRAY_SRC).unwrap();
    use aipl::FfiValue::{Array, Int, Opt};
    assert_eq!(
        e.call_values("maybe", &[Int(1)]).unwrap(),
        Opt(Some(Box::new(Array(vec![Int(1), Int(2)]))))
    );
    assert_eq!(e.call_values("maybe", &[Int(0)]).unwrap(), Opt(None));
}

#[test]
fn call_values_rejects_array_argument() {
    // Arrays marshal *out* but not *in* yet: passing an `Array` argument is a
    // clear error, not a silent misread.
    let e = Engine::compile("pub fn len(xs: i64[]) -> i64 { 0 }").unwrap();
    use aipl::FfiValue::{Array, Int};
    assert!(e
        .call_values("len", &[Array(vec![Int(1), Int(2)])])
        .is_err());
}

#[test]
fn call_values_validates_variant_against_param_type() {
    let src = "\
import { wrapping_add as + } from builtins;
pub fn add(a: i64, b: i64) -> i64 { a + b }
pub fn id(s: str) -> str { s }";
    let e = Engine::compile(src).unwrap();
    use aipl::FfiValue::{Int, Str};
    // Scalars still marshal via `Int`.
    assert_eq!(e.call_values("add", &[Int(2), Int(3)]).unwrap(), Int(5));
    // A `Str` for an `i64` param, or an `Int` for a `str` param, is rejected.
    assert!(e.call_values("add", &[Str("x".into()), Int(1)]).is_err());
    assert!(e.call_values("id", &[Int(0)]).is_err());
}

#[test]
fn compile_file_loads_functions_from_separate_files() {
    // The compiler-in-AIPL direction: helpers live in their own `.aipl` files,
    // a root file imports them, and the FFI loads the root and calls its
    // functions by name — the imported helper is reached transitively.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ffi_fixtures/calc.aipl");
    let e = Engine::compile_file(&root).unwrap();
    assert_eq!(e.call("sum_of_squares", &[3, 4]).unwrap(), 25); // 9 + 16
}

#[test]
fn compile_sources_embeds_separate_files_via_include_str() {
    // The same fixtures `compile_file` loads from disk, compiled instead from
    // in-memory sources (as a host would `include_str!` them). `calc.aipl`
    // imports `from "./mathlib.aipl"`, which resolves by name to the supplied
    // "mathlib.aipl" entry — nothing is read from disk.
    let e = Engine::compile_sources(&[
        ("./calc.aipl", include_str!("ffi_fixtures/calc.aipl")), // root (first)
        ("./mathlib.aipl", include_str!("ffi_fixtures/mathlib.aipl")),
    ])
    .unwrap();
    assert_eq!(e.call("sum_of_squares", &[3, 4]).unwrap(), 25);
}

#[test]
fn compile_sources_rejects_a_missing_module() {
    // calc.aipl imports "mathlib.aipl", which we don't supply.
    let err = Engine::compile_sources(&[(
        "./ffi_fixtures/calc.aipl",
        include_str!("ffi_fixtures/calc.aipl"),
    )]);
    assert_eq!(err.err().expect("Should err").message, "calc.aipl: imported module \"mathlib.aipl\" was not provided to compile_sources. Sources: [\"./ffi_fixtures/calc.aipl\"]");
}

/// Recursively collect `.aipl` files under `dir`.
fn collect_aipl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_aipl(&p, out);
        } else if p.extension().is_some_and(|e| e == "aipl") {
            out.push(p);
        }
    }
}

/// Every `.aipl` file embedded in a compiler crate (used via the FFI) must be
/// well-tested and pass `aipl check`. This enforces the CLAUDE.md rule: each
/// such file carries `.test` blocks, and they all pass.
#[test]
fn compiler_aipl_files_are_tested_and_pass_check() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates");
    let mut files = Vec::new();
    collect_aipl(&crates, &mut files);
    assert!(
        !files.is_empty(),
        "no compiler .aipl files found under {} — did discovery break?",
        crates.display()
    );
    for f in &files {
        let src = std::fs::read_to_string(f).unwrap();
        assert!(
            src.contains(".test("),
            "{} has no `.test` block — aipl functions used in the compiler must be tested",
            f.display()
        );
        let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
            .arg("check")
            .arg(f)
            .output()
            .expect("spawn aipl check");
        assert!(
            out.status.success(),
            "`aipl check {}` failed:\n{}{}",
            f.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

#[test]
fn surfaces_compile_errors() {
    // Body type doesn't match the declared return — a checker error.
    let err = Engine::compile("pub fn f() -> i64 { \"oops\" }");
    assert!(err.is_err());
}
