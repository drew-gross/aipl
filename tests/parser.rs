//! Integration tests for the parser.

use aipl::ast::{
    Expr, ExprKind, FieldDecl, FieldInit, ImportSource, Item, MatchArm, Param, Primitive, Program,
    StructDecl, Type,
};
use aipl::Span;

/// Parse, first installing the (idempotent) parser hooks the dogfooded
/// section-header / raw-string helpers require — there's no native fallback.
fn parse(src: &str) -> Result<Program, aipl::Error> {
    aipl::install_parser_hooks();
    aipl::parse(src)
}

fn fn_item(p: &Program, idx: usize) -> &aipl::ast::Function {
    match &p.items[idx] {
        Item::Fn(f) => f,
        other => panic!("expected fn at index {idx}, got {other:?}"),
    }
}

fn struct_item(p: &Program, idx: usize) -> &StructDecl {
    match &p.items[idx] {
        Item::Struct(s) => s,
        other => panic!("expected struct at index {idx}, got {other:?}"),
    }
}

fn dummy(kind: ExprKind) -> Expr {
    Expr::new(kind, Span::DUMMY)
}

fn num(n: i64) -> Expr {
    dummy(ExprKind::Num(n))
}

fn bool_lit(b: bool) -> Expr {
    dummy(ExprKind::Bool(b))
}

fn ident(s: &str) -> Expr {
    dummy(ExprKind::Ident(s.into()))
}

fn binop(l: Expr, op: char, r: Expr) -> Expr {
    dummy(ExprKind::Binop(Box::new(l), op, Box::new(r)))
}

fn neg(e: Expr) -> Expr {
    dummy(ExprKind::Neg(Box::new(e)))
}

fn not(e: Expr) -> Expr {
    dummy(ExprKind::Not(Box::new(e)))
}

fn call(name: &str, args: Vec<Expr>) -> Expr {
    dummy(ExprKind::Call(name.into(), args, false))
}

fn construct(name: &str, fields: Vec<FieldInit>) -> Expr {
    dummy(ExprKind::Construct(name.into(), fields))
}

fn field(obj: Expr, name: &str) -> Expr {
    dummy(ExprKind::Field(Box::new(obj), name.into()))
}

fn if_expr(cond: Expr, then_b: Expr, else_b: Expr) -> Expr {
    dummy(ExprKind::If(
        Box::new(cond),
        Box::new(then_b),
        Box::new(else_b),
    ))
}

fn i64_ty() -> Type {
    Type::Primitive(Primitive::I64)
}

#[test]
fn empty_program_parses() {
    let p = parse("").unwrap();
    assert!(p.items.is_empty());
}

#[test]
fn simplest_function() {
    let p = parse("fn main() { 0 }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.name, "main");
    assert!(f.params.is_empty());
    assert_eq!(f.return_ty, None);
    assert_eq!(f.body, num(0));
}

#[test]
fn function_with_return_type() {
    let p = parse("fn answer() -> i64 { 42 }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.return_ty, Some(i64_ty()));
    assert_eq!(f.body, num(42));
}

#[test]
fn function_with_one_param() {
    let p = parse("fn id(x: i64) -> i64 { x }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(
        f.params,
        vec![Param {
            name: "x".into(),
            ty: i64_ty(),
            mutable: false,
            variadic: false,
        }]
    );
    assert_eq!(f.body, ident("x"));
}

#[test]
fn function_with_multiple_params() {
    let p = parse("fn add(x: i64, y: i64) -> i64 { x + y }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.params.len(), 2);
    assert_eq!(f.params[0].name, "x");
    assert_eq!(f.params[1].name, "y");
    assert_eq!(f.body, binop(ident("x"), '+', ident("y")));
}

#[test]
fn multiple_functions_in_program() {
    let src = "fn a() { 1 } fn b() { 2 }";
    let p = parse(src).unwrap();
    assert_eq!(p.items.len(), 2);
    assert_eq!(fn_item(&p, 0).name, "a");
    assert_eq!(fn_item(&p, 1).name, "b");
}

#[test]
fn body_respects_operator_precedence() {
    let p = parse("fn f() { 1 + 2 * 3 }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        binop(num(1), '+', binop(num(2), '*', num(3)))
    );
}

#[test]
fn unary_minus_in_body() {
    let p = parse("fn f() { -5 }").unwrap();
    assert_eq!(fn_item(&p, 0).body, neg(num(5)));
}

#[test]
fn whitespace_is_irrelevant() {
    let a = parse("fn   f  (  x  :  i64  )  ->  i64  {  x  }").unwrap();
    let b = parse("fn f(x: i64) -> i64 { x }").unwrap();
    assert_eq!(a, b);
}

#[test]
fn call_with_no_args() {
    let p = parse("fn f() { g() }").unwrap();
    assert_eq!(fn_item(&p, 0).body, call("g", vec![]));
}

#[test]
fn call_with_args() {
    let p = parse("fn f() { add(1, 2) }").unwrap();
    assert_eq!(fn_item(&p, 0).body, call("add", vec![num(1), num(2)]));
}

#[test]
fn nested_calls() {
    let p = parse("fn f() { a(b(1), 2) }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        call("a", vec![call("b", vec![num(1)]), num(2)])
    );
}

#[test]
fn call_in_expression() {
    let p = parse("fn f() { 1 + g(2) }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        binop(num(1), '+', call("g", vec![num(2)]))
    );
}

#[test]
fn unary_minus_on_call() {
    let p = parse("fn f() { -g(1) }").unwrap();
    assert_eq!(fn_item(&p, 0).body, neg(call("g", vec![num(1)])));
}

#[test]
fn simple_if_else() {
    let p = parse("fn f() { if (1) { 2 } else { 3 } }").unwrap();
    assert_eq!(fn_item(&p, 0).body, if_expr(num(1), num(2), num(3)));
}

#[test]
fn if_with_comparison() {
    let p = parse("fn f(x: i64) { if (x < 10) { x } else { 0 } }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        if_expr(binop(ident("x"), '<', num(10)), ident("x"), num(0))
    );
}

#[test]
fn comparison_ops_parse() {
    for (src, code) in [
        ("a == b", 'E'),
        ("a != b", 'N'),
        ("a < b", '<'),
        ("a > b", '>'),
        ("a <= b", 'L'),
        ("a >= b", 'G'),
    ] {
        let p = parse(&format!("fn f() {{ {src} }}")).unwrap();
        assert_eq!(
            fn_item(&p, 0).body,
            binop(ident("a"), code, ident("b")),
            "src: {src}"
        );
    }
}

#[test]
fn comparison_binds_tighter_than_eq() {
    let p = parse("fn f(x: i64, y: i64) { x < y == 0 }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        binop(binop(ident("x"), '<', ident("y")), 'E', num(0))
    );
}

#[test]
fn arithmetic_binds_tighter_than_comparison() {
    let p = parse("fn f(x: i64, y: i64) { x + 1 < y * 2 }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        binop(
            binop(ident("x"), '+', num(1)),
            '<',
            binop(ident("y"), '*', num(2))
        )
    );
}

#[test]
fn nested_if_via_else() {
    let p = parse("fn f(x: i64) { if (x) { 1 } else { if (x) { 2 } else { 3 } } }").unwrap();
    let inner = if_expr(ident("x"), num(2), num(3));
    assert_eq!(fn_item(&p, 0).body, if_expr(ident("x"), num(1), inner));
}

// ---------- struct tests ----------

#[test]
fn empty_struct_declaration() {
    let p = parse("struct Empty {}").unwrap();
    let s = struct_item(&p, 0);
    assert_eq!(s.name, "Empty");
    assert!(s.fields.is_empty());
}

#[test]
fn struct_with_fields() {
    let p = parse("struct Point { x: i64, y: i64 }").unwrap();
    let s = struct_item(&p, 0);
    assert_eq!(s.name, "Point");
    assert_eq!(
        s.fields,
        vec![
            FieldDecl {
                name: "x".into(),
                ty: i64_ty(),
                default: None,
            },
            FieldDecl {
                name: "y".into(),
                ty: i64_ty(),
                default: None,
            },
        ]
    );
}

#[test]
fn struct_and_fn_in_same_program() {
    let p = parse(
        "struct Point { x: i64, y: i64 }
         fn make() -> i64 { 0 }",
    )
    .unwrap();
    assert_eq!(p.items.len(), 2);
    assert_eq!(struct_item(&p, 0).name, "Point");
    assert_eq!(fn_item(&p, 1).name, "make");
}

#[test]
fn struct_construction_parses() {
    let p = parse("fn f() { Point { x: 1, y: 2 } }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        construct(
            "Point",
            vec![
                FieldInit {
                    name: "x".into(),
                    value: num(1)
                },
                FieldInit {
                    name: "y".into(),
                    value: num(2)
                },
            ]
        )
    );
}

#[test]
fn empty_struct_construction_parses() {
    let p = parse("fn f() { Empty {} }").unwrap();
    assert_eq!(fn_item(&p, 0).body, construct("Empty", vec![]));
}

#[test]
fn field_access_parses() {
    let p = parse("fn f(p: Point) { p.x }").unwrap();
    assert_eq!(fn_item(&p, 0).body, field(ident("p"), "x"));
}

#[test]
fn chained_field_access_left_associative() {
    let p = parse("fn f(p: Pair) { p.first.x }").unwrap();
    assert_eq!(fn_item(&p, 0).body, field(field(ident("p"), "first"), "x"));
}

#[test]
fn struct_as_param_type() {
    let p = parse("fn f(p: Point) -> i64 { p.x }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.params[0].ty, Type::Named("Point".into()));
}

#[test]
fn construct_field_value_can_be_expr() {
    let p = parse("fn f(x: i64) { Point { x: x + 1, y: -x } }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        construct(
            "Point",
            vec![
                FieldInit {
                    name: "x".into(),
                    value: binop(ident("x"), '+', num(1)),
                },
                FieldInit {
                    name: "y".into(),
                    value: neg(ident("x")),
                },
            ]
        )
    );
}

#[test]
fn struct_literal_in_if_condition_needs_parens() {
    let p = parse("fn f() { if (Empty {}) { 1 } else { 0 } }");
    assert!(p.is_ok(), "expected parse to succeed: {p:?}");
}

// ---------- bool tests ----------

#[test]
fn bool_literals() {
    let p = parse("fn f() { true }").unwrap();
    assert_eq!(fn_item(&p, 0).body, bool_lit(true));
    let p = parse("fn f() { false }").unwrap();
    assert_eq!(fn_item(&p, 0).body, bool_lit(false));
}

#[test]
fn bang_unary() {
    let p = parse("fn f() { !true }").unwrap();
    assert_eq!(fn_item(&p, 0).body, not(bool_lit(true)));
}

#[test]
fn double_bang() {
    let p = parse("fn f() { !!true }").unwrap();
    assert_eq!(fn_item(&p, 0).body, not(not(bool_lit(true))));
}

#[test]
fn logical_ops_parse() {
    let p = parse("fn f() { true && false }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        binop(bool_lit(true), 'A', bool_lit(false))
    );
    let p = parse("fn f() { true || false }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        binop(bool_lit(true), 'O', bool_lit(false))
    );
}

#[test]
fn and_binds_tighter_than_or() {
    let p = parse("fn f() { true || false && true }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        binop(
            bool_lit(true),
            'O',
            binop(bool_lit(false), 'A', bool_lit(true))
        )
    );
}

#[test]
fn comparison_binds_tighter_than_and() {
    let p = parse("fn f(x: i64, y: i64, a: i64, b: i64) { x < y && a == b }").unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        binop(
            binop(ident("x"), '<', ident("y")),
            'A',
            binop(ident("a"), 'E', ident("b"))
        )
    );
}

// ---------- effect tests ----------

#[test]
fn function_without_effects_has_empty_list() {
    let p = parse("fn f() -> i64 { 0 }").unwrap();
    assert!(fn_item(&p, 0).effects.is_empty());
}

#[test]
fn single_effect_parses() {
    let p = parse("fn f() !prints -> i64 { 0 }").unwrap();
    assert_eq!(fn_item(&p, 0).effects, vec!["prints".to_string()]);
}

#[test]
fn multiple_effects_parse() {
    let p = parse("fn f() !prints !reads -> i64 { 0 }").unwrap();
    assert_eq!(
        fn_item(&p, 0).effects,
        vec!["prints".to_string(), "reads".to_string()]
    );
}

#[test]
fn effects_parse_without_return_type() {
    let p = parse("fn f() !prints { 0 }").unwrap();
    assert_eq!(fn_item(&p, 0).effects, vec!["prints".to_string()]);
    assert_eq!(fn_item(&p, 0).return_ty, None);
}

#[test]
fn bool_as_param_type() {
    let p = parse("fn f(b: bool) -> bool { !b }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.params[0].ty, Type::Primitive(Primitive::Bool));
    assert_eq!(f.return_ty, Some(Type::Primitive(Primitive::Bool)));
}

// ---------- string tests ----------

#[test]
fn string_literal_parses() {
    let p = parse(r#"fn f() { "hello" }"#).unwrap();
    assert_eq!(fn_item(&p, 0).body, dummy(ExprKind::Str("hello".into())));
}

#[test]
fn empty_string_literal_parses() {
    let p = parse(r#"fn f() { "" }"#).unwrap();
    assert_eq!(fn_item(&p, 0).body, dummy(ExprKind::Str("".into())));
}

// Raw-string lexing/processing (trimming, de-denting, verbatim contents) is
// covered end-to-end by `tests/cases/strings/raw_string*.aipl`, where the
// source and resulting output are written the way a user would see them.

#[test]
fn string_in_call() {
    let p = parse(r#"fn f() { print("hi") }"#).unwrap();
    assert_eq!(
        fn_item(&p, 0).body,
        call("print", vec![dummy(ExprKind::Str("hi".into()))])
    );
}

#[test]
fn str_as_param_type() {
    let p = parse("fn f(s: str) -> i64 { 0 }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.params[0].ty, Type::Primitive(Primitive::Str));
}

// ---------- span tests ----------

#[test]
fn num_literal_has_span() {
    let p = parse("fn f() { 42 }").unwrap();
    let body = &fn_item(&p, 0).body;
    // "fn f() { 42 }" — "42" is at bytes 9..11.
    assert_eq!(body.span, Span::new(9, 11));
}

#[test]
fn binop_span_covers_both_operands() {
    let p = parse("fn f() { 1 + 2 }").unwrap();
    let body = &fn_item(&p, 0).body;
    // "1 + 2" spans 9..14.
    assert_eq!(body.span.start, 9);
    assert_eq!(body.span.end, 14);
}

// ---------- comments ----------

#[test]
fn line_comment_is_skipped() {
    let src = r#"
        // top-level comment
        fn f() -> i64 {
            // inside body
            42 // trailing
        }
"#;
    let p = parse(src).unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.body.kind, ExprKind::Num(42));
}

#[test]
fn block_comment_is_skipped() {
    let p = parse("/* hi */ fn f() -> i64 { /* mid */ 7 /* end */ }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.body.kind, ExprKind::Num(7));
}

#[test]
fn block_comments_nest() {
    let p = parse("fn f() -> i64 { /* outer /* inner */ still in outer */ 9 }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.body.kind, ExprKind::Num(9));
}

// ---------- trailing whitespace ----------

#[test]
fn trailing_whitespace_on_code_line_is_rejected() {
    // Spaces before the newline on a code line are an error, and the message
    // names the offense.
    let err = parse("fn main() {}   \nfn g() {}").unwrap_err();
    assert!(
        err.message.contains("trailing whitespace"),
        "unexpected error: {err:?}"
    );
}

#[test]
fn trailing_whitespace_at_end_of_file_is_rejected() {
    // The final line need not end in a newline to be flagged.
    assert!(parse("fn main() {}  ").is_err());
}

#[test]
fn whitespace_only_line_is_rejected() {
    // A line that is *only* spaces still ends in whitespace.
    assert!(parse("fn main() {}\n   \nfn g() {}").is_err());
}

#[test]
fn trailing_whitespace_inside_string_is_rejected() {
    // "even within a string": a line of a multi-line raw string that ends in
    // spaces is rejected just like a code line. (The `   ` is inside the raw
    // string, expressed mid-Rust-line so it survives formatting.)
    let src = "fn f() -> str {\n    \"\"\"\n    foo   \n    \"\"\"\n}";
    assert!(
        parse(src).is_err(),
        "trailing whitespace inside a string should be rejected"
    );
}

#[test]
fn clean_source_parses() {
    // Truly empty lines (no spaces) and a trailing newline are fine.
    assert!(parse("fn main() {}\n\nfn g() {}\n").is_ok());
}

// ---------- string escapes ----------

#[test]
fn escape_sequences_decoded() {
    let p = parse(r#"fn f() -> str { "a\nb\tc\\d\"e" }"#).unwrap();
    let f = fn_item(&p, 0);
    match &f.body.kind {
        ExprKind::Str(s) => assert_eq!(s, "a\nb\tc\\d\"e"),
        other => panic!("expected str literal, got {other:?}"),
    }
}

#[test]
fn escaped_quote_doesnt_close_string() {
    let p = parse(r#"fn f() -> str { "a\"b" }"#).unwrap();
    let f = fn_item(&p, 0);
    match &f.body.kind {
        ExprKind::Str(s) => assert_eq!(s, r#"a"b"#),
        other => panic!("expected str literal, got {other:?}"),
    }
}

#[test]
fn line_comment_does_not_steal_division_operator() {
    // A single `/` mid-expression is division, not the start of a comment.
    let p = parse("fn f(x: i64, y: i64) -> i64 { x / y }").unwrap();
    let f = fn_item(&p, 0);
    match &f.body.kind {
        ExprKind::Binop(_, op, _) => assert_eq!(*op, '/'),
        other => panic!("expected binop, got {other:?}"),
    }
}

// ---------- char literals ----------

#[test]
fn char_literal_basic() {
    let p = parse("fn f() -> char { 'A' }").unwrap();
    match &fn_item(&p, 0).body.kind {
        ExprKind::Char(b) => assert_eq!(*b, b'A'),
        other => panic!("expected char, got {other:?}"),
    }
}

#[test]
fn char_literal_escapes() {
    for (src, expected) in [
        (r"'\n'", b'\n'),
        (r"'\t'", b'\t'),
        (r"'\r'", b'\r'),
        (r"'\\'", b'\\'),
        (r"'\''", b'\''),
        (r#"'\"'"#, b'"'),
    ] {
        let p = parse(&format!("fn f() -> char {{ {src} }}")).unwrap();
        match &fn_item(&p, 0).body.kind {
            ExprKind::Char(b) => assert_eq!(*b, expected, "src: {src}"),
            other => panic!("expected char for {src}, got {other:?}"),
        }
    }
}

// ---------- for-each-char loop ----------

#[test]
fn for_loop_parses() {
    let _ = parse(
        "fn f() -> i64 {
            for (let c : \"abc\") { }
            0
        }",
    )
    .unwrap();
}

#[test]
fn for_body_is_statement_only() {
    // A for body is a statement list with no trailing expression: both a
    // statement-only body and an empty body parse. (Rejection of a trailing
    // expression is covered by the cases framework, which shows the rendered
    // parse error — see tests/cases/loops/err_for_body_*.aipl.)
    parse("fn f(s: str) -> i64 { mut n = 0; for (let c : s) { set n = n + 1; } n }").unwrap();
    parse("fn f(s: str) -> i64 { for (let c : s) {} 0 }").unwrap();
}

#[test]
fn for_loop_iterable_can_be_expr() {
    let _ = parse(
        "fn f(s: str) -> i64 {
            for (let c : \"x\" + s) { }
            0
        }",
    )
    .unwrap();
}

// ---------- match arms ----------

fn match_arms(p: &Program) -> &Vec<MatchArm> {
    match &fn_item(p, 0).body.kind {
        ExprKind::Match(_, arms) => arms,
        other => panic!("expected match expr, got {other:?}"),
    }
}

#[test]
fn match_trailing_comma_optional() {
    // The same arms parse identically with or without a trailing comma
    // after the last arm.
    let without = "fn f(x: i64?) -> i64 { match (x) { some(v) => v, none => 0 } }";
    let with = "fn f(x: i64?) -> i64 { match (x) { some(v) => v, none => 0, } }";
    let p_without = parse(without).unwrap();
    let p_with = parse(with).unwrap();
    assert_eq!(match_arms(&p_without).len(), 2);
    assert_eq!(match_arms(&p_with), match_arms(&p_without));
}

// ---------- trailing commas in comma-separated lists ----------

/// Each pair is (without trailing comma, with trailing comma). A
/// trailing comma after the last element is optional and parses to the
/// exact same AST. (Span-free AST nodes only — imports carry spans that
/// shift with the extra comma, so they're checked separately below.)
#[test]
fn trailing_commas_are_optional() {
    let pairs = [
        // function params
        (
            "fn f(x: i64, y: i64) -> i64 { x }",
            "fn f(x: i64, y: i64,) -> i64 { x }",
        ),
        // call args
        ("fn f() { g(1, 2) }", "fn f() { g(1, 2,) }"),
        // struct field declarations
        (
            "struct P { x: i64, y: i64 }",
            "struct P { x: i64, y: i64, }",
        ),
        // struct literal field inits
        (
            "fn f() { P { x: 1, y: 2 } }",
            "fn f() { P { x: 1, y: 2, } }",
        ),
        // single-element lists
        ("fn f(x: i64) { x }", "fn f(x: i64,) { x }"),
        ("fn f() { g(1) }", "fn f() { g(1,) }"),
    ];
    for (without, with) in pairs {
        let p_without = parse(without).unwrap_or_else(|e| panic!("parse {without:?}: {e:?}"));
        let p_with = parse(with).unwrap_or_else(|e| panic!("parse {with:?}: {e:?}"));
        assert_eq!(
            p_with.items, p_without.items,
            "trailing comma changed the AST:\n  {without}\n  {with}"
        );
    }
}

// ---------- arrays ----------

#[test]
fn array_type_parses() {
    let p = parse("fn f(xs: i64[]) -> i64 { 0 }").unwrap();
    assert_eq!(
        fn_item(&p, 0).params[0].ty,
        Type::Array(Box::new(Type::Primitive(Primitive::I64)))
    );
}

#[test]
fn array_type_of_char_and_bool() {
    let p = parse("fn f(a: char[], b: bool[]) -> i64 { 0 }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(
        f.params[0].ty,
        Type::Array(Box::new(Type::Primitive(Primitive::Char)))
    );
    assert_eq!(
        f.params[1].ty,
        Type::Array(Box::new(Type::Primitive(Primitive::Bool)))
    );
}

#[test]
fn array_literal_parses() {
    let p = parse("fn f() { [1, 2, 3] }").unwrap();
    match &fn_item(&p, 0).body.kind {
        ExprKind::ArrayLit(elems) => {
            assert_eq!(elems.len(), 3);
            assert_eq!(elems[0], num(1));
            assert_eq!(elems[2], num(3));
        }
        other => panic!("expected array literal, got {other:?}"),
    }
}

#[test]
fn empty_array_literal_parses() {
    let p = parse("fn f() { [] }").unwrap();
    match &fn_item(&p, 0).body.kind {
        ExprKind::ArrayLit(elems) => assert!(elems.is_empty()),
        other => panic!("expected empty array literal, got {other:?}"),
    }
}

#[test]
fn array_literal_allows_trailing_comma() {
    let with = parse("fn f() { [1, 2,] }").unwrap();
    let without = parse("fn f() { [1, 2] }").unwrap();
    assert_eq!(fn_item(&with, 0).body, fn_item(&without, 0).body);
}

#[test]
fn index_parses() {
    let p = parse("fn f(xs: i64[]) -> i64 { xs[0] }").unwrap();
    match &fn_item(&p, 0).body.kind {
        ExprKind::Index(obj, idx) => {
            assert_eq!(obj.kind, ExprKind::Ident("xs".into()));
            assert_eq!(**idx, num(0));
        }
        other => panic!("expected index, got {other:?}"),
    }
}

#[test]
fn index_accepts_expression_subscript() {
    // The subscript is a full expression, not just a literal.
    let p = parse("fn f(xs: i64[], i: i64) -> i64 { xs[i + 1] }").unwrap();
    match &fn_item(&p, 0).body.kind {
        ExprKind::Index(_, idx) => {
            assert_eq!(**idx, binop(ident("i"), '+', num(1)));
        }
        other => panic!("expected index, got {other:?}"),
    }
}

// ---------- generic type parameters ----------

#[test]
fn generic_type_params_parse() {
    let p = parse("fn value_or<T: any>(x: T?, d: T) -> T { d }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(f.type_params, vec!["T".to_string()]);
    assert_eq!(
        f.params[0].ty,
        Type::Optional(Box::new(Type::Named("T".into())))
    );
    assert_eq!(f.params[1].ty, Type::Named("T".into()));
    assert_eq!(f.return_ty, Some(Type::Named("T".into())));
}

#[test]
fn multiple_type_params_parse() {
    let p = parse("fn f<T: any, U: any>(a: T, b: U) -> i64 { 0 }").unwrap();
    assert_eq!(
        fn_item(&p, 0).type_params,
        vec!["T".to_string(), "U".to_string()]
    );
}

#[test]
fn function_without_type_params_has_none() {
    // The `<..>` list is optional and `<`/`>` still parse as comparisons.
    let p = parse("fn f(x: i64) -> bool { x < 3 }").unwrap();
    assert!(fn_item(&p, 0).type_params.is_empty());
    assert_eq!(fn_item(&p, 0).body, binop(ident("x"), '<', num(3)));
}

// ---------- builtin imports ----------

fn import_at<'a>(p: &'a Program, idx: usize) -> &'a aipl::ast::ImportDecl {
    match &p.items[idx] {
        Item::Import(i) => i,
        other => panic!("expected import at {idx}, got {other:?}"),
    }
}

#[test]
fn path_import_parses_as_path_source() {
    let p = parse("import { a, b } from \"m\"; fn f() { 0 }").unwrap();
    let imp = import_at(&p, 0);
    assert_eq!(
        imp.names
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    match &imp.source {
        ImportSource::Path { path, .. } => assert_eq!(path, "m"),
        other => panic!("expected path source, got {other:?}"),
    }
}

#[test]
fn builtins_import_parses_as_builtins_source() {
    let p = parse("import { len, push } from builtins; fn f() { 0 }").unwrap();
    let imp = import_at(&p, 0);
    assert_eq!(
        imp.names
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        ["len", "push"]
    );
    assert!(matches!(imp.source, ImportSource::Builtins { .. }));
}

#[test]
fn builtins_import_allows_trailing_comma() {
    let p = parse("import { len, } from builtins; fn f() { 0 }").unwrap();
    assert!(matches!(
        import_at(&p, 0).source,
        ImportSource::Builtins { .. }
    ));
}

#[test]
fn import_trailing_comma_optional() {
    // ImportDecl carries source spans that shift with the extra comma,
    // so compare the imported names rather than the whole node.
    let names = |src: &str| -> Vec<String> {
        let p = parse(src).unwrap_or_else(|e| panic!("parse {src:?}: {e:?}"));
        match &p.items[0] {
            Item::Import(i) => i.names.iter().map(|n| n.name.clone()).collect(),
            other => panic!("expected import, got {other:?}"),
        }
    };
    assert_eq!(
        names("import { a, b, } from \"m\"; fn f() { 0 }"),
        vec!["a".to_string(), "b".to_string()],
    );
    assert_eq!(
        names("import { a, } from \"m\"; fn f() { 0 }"),
        vec!["a".to_string()],
    );
}

#[test]
fn parses_function_type_parameter() {
    let p = parse("fn apply(f: (i64) -> i64, x: i64) -> i64 { f(x) }").unwrap();
    let f = fn_item(&p, 0);
    assert_eq!(
        f.params[0].ty,
        Type::Fn(
            vec![Type::Primitive(Primitive::I64)],
            Box::new(Type::Primitive(Primitive::I64))
        )
    );
    // Zero-arg and multi-arg function types parse too.
    parse("fn g(h: () -> i64) -> i64 { h() }").unwrap();
    parse("fn g(h: (i64, bool) -> str) -> i64 { 0 }").unwrap();
}

#[test]
fn parses_lambda_argument() {
    let p = parse("fn main() -> i64 { apply(|x| x + 1, 5) }").unwrap();
    let f = fn_item(&p, 0);
    let ExprKind::Call(name, args, _) = &f.body.kind else {
        panic!("expected call, got {:?}", f.body.kind)
    };
    assert_eq!(name, "apply");
    let ExprKind::Lambda(params, body) = &args[0].kind else {
        panic!("expected lambda, got {:?}", args[0].kind)
    };
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name, "x");
    assert!(params[0].ty.is_none());
    assert!(matches!(&body.kind, ExprKind::Binop(..)));
    assert!(matches!(&args[1].kind, ExprKind::Num(5)));
}

#[test]
fn parses_lambda_forms() {
    // No params, multiple params, an explicit param type, and a block body.
    parse("fn main() -> i64 { f(|| 0) }").unwrap();
    parse("fn main() -> i64 { f(|x, y| x + y) }").unwrap();
    parse("fn main() -> i64 { f(|x: i64| x) }").unwrap();
    parse("fn main() -> i64 { f(|x| { let d = x * 2; d + 1 }) }").unwrap();
}

#[test]
fn or_token_serves_both_roles() {
    // `||` is still infix logical-or inside an expression...
    let p = parse("fn main() -> bool { a || b }").unwrap();
    let f = fn_item(&p, 0);
    assert!(
        matches!(&f.body.kind, ExprKind::Binop(_, 'O', _)),
        "expected logical-or binop, got {:?}",
        f.body.kind
    );
    // ...and the lead of a no-arg lambda in argument position.
    let p = parse("fn main() -> i64 { f(|| a || b) }").unwrap();
    let f = fn_item(&p, 0);
    let ExprKind::Call(_, args, _) = &f.body.kind else {
        panic!("expected call")
    };
    let ExprKind::Lambda(params, body) = &args[0].kind else {
        panic!("expected lambda, got {:?}", args[0].kind)
    };
    assert!(params.is_empty());
    // The body is the `a || b` logical-or.
    assert!(matches!(&body.kind, ExprKind::Binop(_, 'O', _)));
}
