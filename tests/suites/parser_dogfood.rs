//! The AIPL grammar written in AIPL (`crates/aipl-codegen/src/grammar_aipl.aipl`)
//! parses the same language as the gazelle LR(1) grammar in `aipl-parser`.
//!
//! This is the differential test `PARSER_LIBRARY.md` exists for, and
//! it is the same shape as [`lexer_dogfood::dogfood_lex_hook_matches_fresh_compile_on_corpus`]:
//! run both implementations over every `.aipl` in the repository and require
//! them to agree, file by file. Agreement here is *acceptance* — the AIPL
//! grammar builds no AST yet — plus, for every file that parses, the
//! losslessness property the concrete tree exists to provide: concatenating its
//! leaf spans returns the source byte for byte.
//!
//! Rejection is half the comparison. The corpus carries ~100 deliberate
//! syntax-error fixtures, and a grammar that accepted everything would agree
//! with gazelle on none of them.

use aipl::{Engine, FfiValue};
use aipl_syntax::ast::ExprKind;
use std::fs;
use std::path::{Path, PathBuf};

/// Run `f` on a 64 MB-stack worker. Compiling the grammar's dependency closure
/// (it reaches the parser library *and* the whole lexer) recurses deeper than a
/// test thread's default stack allows — the same reason `lexer_dogfood.rs` and
/// `dogfood_ir.rs` spawn one.
fn on_big_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(f)
        .expect("spawn worker")
        .join()
        .expect("worker panicked");
}

/// Compile `grammar_aipl.aipl` and everything it imports, straight from the
/// working tree. It is deliberately *not* in `DOGFOOD_SOURCE_FILES` — nothing in
/// the compiler runs on it yet — so there is no checked-in IR and no hook: the
/// engine is built here, once per test.
fn compile_grammar() -> Engine {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("crates")
        .join("aipl-codegen")
        .join("src")
        .join("grammar_aipl.aipl");
    Engine::compile_file(&path).unwrap_or_else(|e| panic!("compile {}: {e:?}", path.display()))
}

/// What the AIPL grammar makes of `src`: empty when it parses and its tree
/// reconstructs the source, else a line starting `error:` or `lossless:`.
fn report(engine: &Engine, src: &str) -> String {
    match engine.call_values("aipl_parse_report", &[FfiValue::Str(src.to_string())]) {
        Ok(FfiValue::Str(s)) => s,
        other => panic!("aipl_parse_report(): {other:?}"),
    }
}

/// Every `.aipl` at or below `dir`, recursively.
fn collect_aipl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_aipl(&path, out);
        } else if path.extension().is_some_and(|e| e == "aipl") {
            out.push(path);
        }
    }
}

/// The corpus files `aipl_parser::parse` rejects in a **build action** rather
/// than in its grammar, paired with a fragment of the message that says so.
///
/// Gazelle's grammar and its semantic checks arrive through one entry point, so
/// `parse` returning `Err` does not always mean "this is not AIPL". Each of
/// these parses fine under the LR grammar *and* under this one, and is then
/// refused by the code that builds the AST — the gazelle grammar's own comments
/// say as much ("validated in the build action"). They are the exact set of
/// checks Stage 5 will have to reproduce when this grammar grows `build`
/// functions, which is why they are listed rather than waved through: the
/// message fragment pins each entry to the check it names, so a file that starts
/// failing for a different reason stops matching and fails the test.
const BUILD_ACTION_REJECTS: &[(&str, &str)] = &[
    // `..` rides on `arg`, shared by calls, array literals and array patterns;
    // only the array literal may keep it.
    (
        "tests/cases/arrays/spread/err_spread_in_call.aipl",
        "spread is only allowed in an array literal",
    ),
    (
        "tests/cases/arrays/spread/err_spread_in_pattern.aipl",
        "spread is only allowed in an array literal",
    ),
    // One `#{ .. }` production covers sets and dicts; the builder picks which.
    (
        "tests/cases/dicts/err_mixed_literal.aipl",
        "can't mix set elements and",
    ),
    // `fn_attr` accepts `.NAME(block)` and `.NAME("str")`; which names take
    // which argument, and that none repeats, is the action's business. The
    // string form is dead syntax kept only so the action can refuse it by name.
    (
        "tests/cases/docs/err_doc_attr.aipl",
        "no longer a function attribute",
    ),
    (
        "tests/cases/docs/err_test_wrong_arg.aipl",
        "`.test` takes a `{ .. }` block",
    ),
    (
        "tests/cases/docs/err_unknown_attr.aipl",
        "unknown function attribute",
    ),
    // Both grammars put the `# ..` lines on `item`, `import` included; that an
    // import has nothing to document is the action's rule.
    (
        "tests/cases/docs/err_doc_on_import.aipl",
        "import cannot be documented",
    ),
    // `T: bound` is `IDENT COLON IDENT` to the grammar; the bound names are the
    // action's.
    (
        "tests/cases/generics/errors/err_unknown_type_param_bound.aipl",
        "unknown type parameter bound",
    ),
    // Alternatives of one arm must bind the same names — a property of the
    // patterns, not of the shape they are written in.
    (
        "tests/cases/match_arms/err_alternation_binding_mismatch.aipl",
        "must bind the same names",
    ),
    // The struct-literal body shorthand needs a struct return type to name,
    // which the grammar cannot see.
    (
        "tests/cases/structs/errors/err_body_shorthand_no_return_type.aipl",
        "needs a struct return type",
    ),
    (
        "tests/cases/structs/errors/err_body_shorthand_non_struct.aipl",
        "needs a struct return type",
    ),
];

/// Whether any line of `src` ends in a space or tab.
///
/// `aipl_parser::parse` rejects those *before* it parses — a whitespace rule,
/// not a grammar rule — so a fixture that trips it says nothing about whether
/// the two grammars agree, and is compared for nothing else.
fn has_trailing_whitespace(src: &str) -> bool {
    src.lines().any(|l| l.ends_with(' ') || l.ends_with('\t'))
}

#[test]
fn aipl_grammar_matches_gazelle_on_corpus() {
    on_big_stack(|| {
        aipl::install_parser_hooks();
        let engine = compile_grammar();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for sub in ["tests/cases", "examples", "crates"] {
            collect_aipl(&root.join(sub), &mut files);
        }
        files.sort();
        assert!(
            files.len() > 400,
            "corpus went missing? found {} files",
            files.len()
        );

        let mut disagreements: Vec<String> = Vec::new();
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        let mut skipped = 0usize;
        let mut build_action: Vec<&str> = Vec::new();
        for f in &files {
            let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
            let full = fs::read_to_string(f).expect("read case file");
            let source = aipl::strip_test_sections(&full);
            if has_trailing_whitespace(source) {
                skipped += 1;
                continue;
            }
            let gazelle = aipl::parse(&full);
            let ours = report(&engine, &full);
            match (gazelle.is_ok(), ours.is_empty()) {
                (true, true) => accepted += 1,
                (false, false) => rejected += 1,
                (true, false) => disagreements.push(format!(
                    "{rel}: gazelle parsed it, the AIPL grammar did not — {ours}"
                )),
                (false, true) => {
                    let why = gazelle.err().map(|e| e.to_string()).unwrap_or_default();
                    match BUILD_ACTION_REJECTS.iter().find(|(f, _)| *f == rel) {
                        Some((_, fragment)) if why.contains(fragment) => {
                            build_action.push(fragment)
                        }
                        Some((_, fragment)) => disagreements.push(format!(
                            "{rel}: listed as a build-action rejection about {fragment:?}, \
                             but gazelle refused it for another reason — {why}"
                        )),
                        None => disagreements.push(format!(
                            "{rel}: the AIPL grammar parsed it, gazelle did not — {why}"
                        )),
                    }
                }
            }
        }

        assert!(
            disagreements.is_empty(),
            "{} of {} files disagree ({accepted} agreed-accept, {rejected} agreed-reject, \
             {skipped} skipped for trailing whitespace):\n{}",
            disagreements.len(),
            files.len(),
            disagreements
                .iter()
                .take(25)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            accepted > 400,
            "expected most of the corpus to parse; only {accepted} did"
        );
        assert_eq!(
            build_action.len(),
            BUILD_ACTION_REJECTS.len(),
            "every listed build-action rejection must still be one — {} of {} matched",
            build_action.len(),
            BUILD_ACTION_REJECTS.len()
        );
        assert!(
            rejected > 20,
            "expected the corpus's syntax-error fixtures to be rejected by both; \
             only {rejected} were. A grammar that accepts everything agrees with nothing."
        );
    });
}

/// Sources chosen to exercise the places two grammars can agree about
/// *acceptance* and still disagree about *shape*: precedence, associativity,
/// ordered choice, and the bracket groups that carry a list's layout.
///
/// A fixture rather than the corpus, deliberately. The sweep was tried and
/// abandoned twice — see `PARSER_LIBRARY.md` — because a second FFI call per
/// file re-parses it, doubling a differential that already takes over a minute,
/// and the spans of a compiler-sized source cross as a six-figure array of
/// numbers. These are a fixed cost that catches the same class of bug: a
/// precedence table read backwards, an ordered choice that commits too early, a
/// group whose brackets land on the wrong node.
const SHAPE_FIXTURES: &[&str] = &[
    // Precedence across every level, so a table read in the wrong order shows.
    "fn f(a: i64, b: i64, c: i64) -> i64 { a + b * c }",
    "fn f(a: i64, b: i64, c: i64) -> i64 { a * b + c }",
    "fn f(a: i64, b: i64, c: i64) -> bool { a + b < c }",
    "fn f(a: bool, b: bool, c: bool) -> bool { a || b && c }",
    "fn f(a: i64, b: i64, c: i64) -> bool { a < b == b < c }",
    // Associativity: `-` groups left, so `a - b - c` is `(a - b) - c`.
    "fn f(a: i64, b: i64, c: i64) -> i64 { a - b - c }",
    "fn f(a: i64, b: i64, c: i64) -> i64 { a / b / c }",
    // Parentheses against the same expression unparenthesized.
    "fn f(a: i64, b: i64, c: i64) -> i64 { (a + b) * c }",
    "fn f(a: i64, b: i64, c: i64) -> i64 { a + (b * c) }",
    // Unary against binary, and a unary on a call.
    "fn f(a: i64, b: i64) -> i64 { -a + b }",
    "fn f(a: bool, b: bool) -> bool { !a || b }",
    // Postfix chains: field, index, slice, call, `?` — the ordered-choice pile.
    "fn f(xs: i64[]) -> i64? { xs[0] }",
    "fn f(xs: i64[]) -> i64[] { xs[1..2] }",
    "fn f(s: str) -> u64 { s.len() }",
    "fn f(xs: i64[][]) -> i64? { xs[0].value_or([])[1] }",
    // Groups: a bracketed list, a nested one, and a block.
    "fn f() -> i64[] { [1, 2, 3] }",
    "fn f() -> i64[][] { [[1], [2, 3]] }",
    "fn f(a: i64) -> i64 { if (a < 1) { 2 } else { 3 } }",
    // A call whose arguments are themselves expressions with precedence.
    "fn g(x: i64, y: i64) -> i64 { x }\nfn f(a: i64, b: i64) -> i64 { g(a + b, a * b) }",
    // A match, where arms are ordered choice at the statement level.
    "fn f(x: i64?) -> i64 { match (x) { some(v) => v + 1, none => 0 } }",
    // Longer chains, where a left-associative table read right-to-left shows up
    // as a differently shaped spine rather than as a single swapped pair.
    "fn f(a: i64, b: i64, c: i64, d: i64) -> i64 { a + b + c + d }",
    "fn f(a: i64, b: i64, c: i64, d: i64) -> i64 { a - b - c - d }",
    "fn f(a: i64, b: i64, c: i64, d: i64) -> i64 { a + b - c + d }",
    "fn f(a: i64, b: i64, c: i64, d: i64) -> i64 { a * b + c * d }",
    "fn f(a: i64, b: i64, c: i64, d: i64) -> i64 { a + b * c - d }",
    "fn f(a: i64, b: i64, c: i64, d: i64) -> i64 { a / b * c / d }",
    "fn f(a: i64, b: i64, c: i64, d: i64) -> i64 { a % b + c % d }",
    // Every band of the table against its neighbours, so an off-by-one level
    // shows: arithmetic against comparison, comparison against equality,
    // equality against the logical connectives.
    "fn f(a: i64, b: i64, c: i64, d: i64) -> bool { a + b < c * d }",
    "fn f(a: i64, b: i64, c: i64, d: i64) -> bool { a < b == c < d }",
    "fn f(a: bool, b: bool, c: bool, d: bool) -> bool { a && b || c && d }",
    "fn f(a: i64, b: i64, c: bool) -> bool { a < b && c }",
    "fn f(a: i64, b: i64, c: bool) -> bool { c || a == b }",
    // Concatenation, which sits at its own level.
    "fn f(a: str, b: str, c: str) -> str { a +++ b +++ c }",
];

/// The AIPL grammar's tree groups expressions the way gazelle's does.
///
/// Acceptance agreement (above) says the two admit the same language; it says
/// nothing about *structure*, and structure is what a lowering will read. Two
/// grammars can accept `a + b * c` and disagree about which operator binds
/// tighter, and nothing in this file would have noticed.
///
/// The comparison is **subset, not equality**: every span gazelle records for an
/// expression must be a token span of some node in the CST. The concrete tree
/// keeps productions the AST has no node for — `postfix`, `atom`, the plumbing
/// ordered choice needs — so it always has strictly more spans. An extra one is
/// noise; a missing one means the two disagree about where an expression starts
/// or ends, which is what a precedence or associativity bug looks like.
#[test]
fn aipl_grammar_groups_expressions_like_gazelle() {
    on_big_stack(|| {
        let engine = compile_grammar();
        let mut disagreements: Vec<String> = Vec::new();
        let mut compared = 0usize;

        for src in SHAPE_FIXTURES {
            let program = match aipl::parse(src) {
                Ok(p) => p,
                Err(e) => panic!("fixture does not parse with gazelle: {src:?}: {e}"),
            };
            let spans =
                match engine.call_values("aipl_node_spans", &[FfiValue::Str(src.to_string())]) {
                    Ok(FfiValue::Res(Ok(boxed))) => match *boxed {
                        FfiValue::Array(v) => v,
                        other => panic!("aipl_node_spans({src:?}) is not an array: {other:?}"),
                    },
                    other => panic!("aipl_node_spans({src:?}): {other:?}"),
                };
            let mut cst: Vec<(usize, usize)> = Vec::with_capacity(spans.len() / 2);
            for pair in spans.chunks(2) {
                let [FfiValue::Int(lo), FfiValue::Int(hi)] = pair else {
                    panic!("aipl_node_spans({src:?}) is not pairs of ints: {pair:?}");
                };
                cst.push((*lo as usize, *hi as usize));
            }

            aipl_syntax::each_expr(&program, &mut |e| {
                // Operator applications only, and that is the point rather than
                // a concession. Precedence and associativity are what two
                // grammars can disagree about while accepting the same text, and
                // an operator call is where they are decided — `a + b * c` is
                // one grouping or the other, and its span runs operand to
                // operand either way.
                //
                // Every other shape carries a span gazelle keeps for diagnostics
                // rather than as a record of extent, and comparing those means
                // chasing conventions instead of grammar: a bracketed form's
                // span stops before its closing bracket (`[1, 2, 3]` spans
                // `[1, 2, 3`), a call's before its parens — the same thing the
                // lint driver's `spans_its_text` documents from the other side —
                // and unary `-`/`!` leave their own operator out. None of that
                // is a disagreement about shape, and none of it would be caught
                // by looking harder.
                let ExprKind::Call(callee, args, _) = &e.kind else {
                    return;
                };
                if args.len() != 2 || !aipl_syntax::is_operator_name(callee) {
                    return;
                }
                if e.span.start >= e.span.end {
                    return;
                }
                // An operator's span runs from its left operand's start to its
                // right operand's end, which is the true extent only while both
                // operands record theirs. A *parenthesized* operand does not —
                // its span is what is inside the parens — and neither does a
                // unary `-`/`!`, which leaves its own operator out. Either way
                // the operator above it inherits a span one character short, and
                // the mismatch says nothing about grouping.
                //
                // Detected by looking at the neighbouring byte rather than at
                // the AST, which records no parentheses at all. It over-skips —
                // the `+` in `f(a + b)` is faithful and is skipped anyway — and
                // that only ever costs coverage. The inner assertions survive:
                // in `(a + b) * c` the `*` is skipped while the `+` inside it is
                // compared, and it is the `+` that says which way the grouping
                // went.
                let before = src.as_bytes().get(e.span.start.wrapping_sub(1)).copied();
                let after = src.as_bytes().get(e.span.end).copied();
                let delimited = e.span.start > 0
                    && matches!(before, Some(b'(') | Some(b'-') | Some(b'!'))
                    || matches!(after, Some(b')'));
                if delimited {
                    return;
                }
                compared += 1;
                if !cst.contains(&(e.span.start, e.span.end)) {
                    disagreements.push(format!(
                        "{src:?}: gazelle groups {callee:?} over {:?} at {}..{}, which is \
                         no node of the AIPL grammar's tree",
                        &src[e.span.clone()],
                        e.span.start,
                        e.span.end
                    ));
                }
            });
        }

        assert!(
            disagreements.is_empty(),
            "{} expression span(s) of {compared} have no counterpart:\n{}",
            disagreements.len(),
            disagreements
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        // The floor is a guard against the comparison quietly emptying out — a
        // skip rule that widened, or a fixture list that lost its operators,
        // would leave this passing on nothing. It sits below the 52 the
        // fixtures currently reach so that adding one is not a chore.
        assert!(
            compared >= 40,
            "expected the fixtures to exercise a good many operator applications; \
             only {compared} did"
        );
    });
}

/// Sources that exercise the *declaration* half of the grammar, which
/// `SHAPE_FIXTURES` leaves almost untouched: it is an expression list, chosen
/// for precedence and grouping.
///
/// These are here for the lowering rather than for shape — every parameter
/// form, both destructuring statements, the three `for` loops, the nine `set`
/// spellings, the body shorthand, and a doc comment, so the 54 `build`
/// functions are asked about the syntax a real file contains rather than only
/// about the pieces their own assertions name.
const DECL_FIXTURES: &[&str] = &[
    "import { equal as ==, len, - } from builtins;\nimport { P } from \"./p.aipl\";",
    "# A point.\n# Two lines of it.\nstruct P { x: i64, y: i64 = 0 }",
    "struct B<T: any, U: variant> { v: T, w: U }",
    "variant V = A | B(i64) | C(x: i64, y: i64 = 1)",
    "variant W<T: any> = | Some(T) | Nothing",
    "fn f(mut xs: i64[], y: str = \"z\", n: i64*, k?: u64 = none) { }",
    "fn f(Point { x, y }) -> i64 { x }",
    "pub fn f<T: ord>(a: T) -> T { a }",
    "fn f() !prints !reads { g(); }",
    "fn f(v: i64) -> P { v, next: none }",
    "fn f(v: P) -> P { ..v, x: 1 }",
    "fn f() -> i64 { 1 }.test({ assert(f() == 1); })",
    "fn f(p: P) -> i64 { let (a, b) = p; a }",
    "fn f(p: P) -> i64 { let P { x, y } = p; x }",
    "fn f(p: P) -> i64 { mut P { x } = p; x }",
    "fn f(xs: i64[]) { for (let x : xs) { g(x); } }",
    "fn f(xs: i64[]) { for (let i, x : xs) { g(i); } }",
    "fn f(ps: P[]) { for (let (a, b) : ps) { g(a); } }",
    "fn f(mut n: i64) { set n++; set n--; set n += 1; set n -= 1; set n *= 2; set n /= 2; }",
    "fn f(mut xs: i64[], mut p: P) { set xs.push(1); set p.x = 1; set p.a.b = 2; }",
    "fn f(b: bool) -> i64 { while (b) { g(); } return 1; }",
    "fn f(v: V) -> i64 { match (v) { A(x) | B(x) => x, C(..) => 1, _ => 0 } }",
    "fn f(o: i64?) -> i64 { if (let some(v) = o) { v } else { 0 } }",
    "fn f() { shim prints { print = p } { g(); } }",
    "fn f(x: i64) -> str { `a{x}b` }",
    "fn f() -> #{str: i64} { #{\"a\": 1} }",
];

/// The AIPL grammar builds the same `Program` gazelle does.
///
/// This is the whole of stage 5 in one assertion: lex, parse, lower, marshal,
/// rebuild — and the result has to equal what the parser being replaced
/// produces, node for node. Acceptance agreement says the two admit the same
/// language and shape agreement says they group expressions alike; this says
/// they *mean* the same thing, which is the only claim that lets gazelle go.
///
/// **Spans are zeroed on both sides before comparing.** Gazelle's are kept for
/// diagnostics rather than as a record of extent — a bracketed form's stops
/// before its closing bracket, a unary leaves its own operator out — so they are
/// a convention to match rather than a fact to agree about, and matching them is
/// 5e's job. `Expr::ty` and `Expr::value_span` go with them: neither is set by
/// the AIPL side, and `value_span` is one of 5f's side-channels.
#[test]
fn aipl_grammar_builds_the_same_ast_as_gazelle() {
    on_big_stack(|| {
        let engine = compile_grammar();
        let mut mismatches: Vec<String> = Vec::new();

        for src in SHAPE_FIXTURES.iter().chain(DECL_FIXTURES.iter()) {
            let mut want = match aipl::parse(src) {
                Ok(p) => p,
                Err(e) => panic!("fixture does not parse with gazelle: {src:?}: {e}"),
            };
            let value = match engine.call_values("aipl_program", &[FfiValue::Str(src.to_string())])
            {
                Ok(v) => v,
                Err(e) => panic!("aipl_program({src:?}): {e:?}"),
            };
            let mut got = match aipl::ffi_ast::program_from_ffi(&value) {
                Ok(p) => p,
                Err(e) => {
                    mismatches.push(format!("{src:?}: did not rebuild: {e}"));
                    continue;
                }
            };
            // The two rewrites that follow a parse but are no part of one.
            // `aipl::parse` runs them for gazelle; running them here is what
            // compares the two sides at the same stage, and is what stage 5g's
            // entry point will do for real.
            aipl_parser::post_parse(&mut got, src);
            zero_spans(&mut want);
            zero_spans(&mut got);
            if want != got {
                mismatches.push(format!("{src:?}\n  gazelle: {want:?}\n  aipl:    {got:?}"));
            }
        }

        assert!(
            mismatches.is_empty(),
            "{} of {} fixture(s) disagree:\n{}",
            mismatches.len(),
            SHAPE_FIXTURES.len() + DECL_FIXTURES.len(),
            mismatches.join("\n")
        );
    });
}

/// Every span in `p` set to `0..0`, and the two `Expr` fields the parser leaves
/// to later passes cleared — so `==` compares structure and names only.
fn zero_spans(p: &mut aipl_syntax::ast::Program) {
    use aipl_syntax::ast::{ImportSource, Item};
    for item in &mut p.items {
        match item {
            Item::Fn(f) => {
                for param in &mut f.sig.params {
                    strip_temp(&mut param.name);
                    if let Some(d) = &mut param.default {
                        zero_expr(d);
                    }
                }
                zero_expr(&mut f.body);
                if let Some(t) = &mut f.test_body {
                    zero_expr(t);
                }
            }
            Item::Struct(s) => {
                for fd in &mut s.fields {
                    if let Some(d) = &mut fd.default {
                        zero_expr(d);
                    }
                }
            }
            Item::Variant(v) => {
                for case in &mut v.cases {
                    for slot in &mut case.payload {
                        if let Some(d) = &mut slot.default {
                            zero_expr(d);
                        }
                    }
                }
            }
            Item::Import(d) => {
                for n in &mut d.names {
                    n.span = 0..0;
                }
                match &mut d.source {
                    ImportSource::Path { span, .. } | ImportSource::Builtins { span } => {
                        *span = 0..0
                    }
                }
            }
        }
    }
}

/// A synthetic binding's name without the span it was numbered from. Leaves
/// every name a user could have written alone: all four prefixes are reserved
/// (`$` is not in the identifier set).
fn strip_temp(name: &mut String) {
    for prefix in ["__tpat$", "__spat$", "__fpat$", "__idx$"] {
        if name.starts_with(prefix) {
            *name = prefix.to_string();
            return;
        }
    }
}

fn zero_expr(e: &mut aipl_syntax::ast::Expr) {
    use aipl_syntax::ast::ExprKind as K;
    e.span = 0..0;
    e.ty = None;
    e.value_span = None;
    match &mut e.kind {
        // `bake_asserts` writes the *text* a condition's span covers into the
        // baked location string, so a span difference surfaces here as a value
        // rather than as a diagnostic. It is the same difference: gazelle's
        // span for a bracketed form stops before its closing bracket, so its
        // string says `[true, false` where the AIPL parser's says
        // `[true, false]`. Cleared with the spans, and for the same reason —
        // matching gazelle's convention is 5e's job.
        K::Call(name, args, _) if name == "__assert" && args.len() == 2 => {
            args[1].kind = K::Str(String::new());
        }
        K::Match(_, arms) => {
            for a in arms {
                a.span = 0..0;
            }
        }
        K::IfLet(arm, ..) => arm.span = 0..0,
        K::Lambda(params, _) => {
            for param in params {
                param.span = 0..0;
            }
        }
        // The synthetic bindings a desugaring introduces are named after the
        // span they came from, so they are span renderings too — and the same
        // convention gap shows: gazelle spans a tuple literal from its first
        // element, the AIPL parser from its `(`, so one says `__tpat$220` where
        // the other says `__tpat$219`. The names are unique either way and
        // nothing reads them; only the digits differ, so only the digits go.
        K::Let(name, ..) | K::LetMut(name, ..) | K::For(name, ..) | K::Ident(name) => {
            strip_temp(name)
        }
        _ => {}
    }
    for child in aipl_syntax::each_subexpr_mut(e) {
        zero_expr(child);
    }
}

/// The same comparison as the fixtures, over real corpus files.
///
/// The fixtures are chosen; these are not, which is the point — a lowering can
/// be right about every shape someone thought to write down and wrong about the
/// combination a real file happens to contain. The differential is the strong
/// one (whole `Program`s, node for node), so the only question is how much of
/// the corpus it can afford.
///
/// **Capped by size, deliberately.** Each file costs a parse on both sides plus
/// a whole AST across the FFI as a nested `FfiValue`; the compiler sources at
/// the large end of the corpus are thousands of lines and their trees cross as
/// six-figure value graphs. `tests/cases/` is where the small files are, and
/// they are also the ones written to exercise one language feature each — so
/// the cap costs breadth of *size*, not breadth of syntax. Files gazelle
/// rejects are skipped: a syntax-error fixture has no AST to compare.
#[test]
fn aipl_grammar_builds_the_same_ast_on_small_corpus_files() {
    on_big_stack(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        collect_aipl(&root.join("tests").join("cases"), &mut files);
        files.sort();

        let engine = compile_grammar();
        let mut mismatches: Vec<String> = Vec::new();
        let mut compared = 0usize;

        for path in &files {
            let Ok(source) = fs::read_to_string(path) else {
                continue;
            };
            if source.len() > 2_000 {
                continue;
            }
            // Spans are relative to the source with its expected-output
            // sections stripped, which is what both parsers see.
            let src = aipl_parser::strip_test_sections(&source);
            let Ok(mut want) = aipl::parse(&source) else {
                continue;
            };
            let value = match engine.call_values("aipl_program", &[FfiValue::Str(source.clone())]) {
                Ok(v) => v,
                Err(e) => panic!("aipl_program({}): {e:?}", path.display()),
            };
            let mut got = match aipl::ffi_ast::program_from_ffi(&value) {
                Ok(p) => p,
                Err(e) => {
                    mismatches.push(format!("{}: did not rebuild: {e}", path.display()));
                    continue;
                }
            };
            aipl_parser::post_parse(&mut got, src);
            zero_spans(&mut want);
            zero_spans(&mut got);
            compared += 1;
            if want != got {
                mismatches.push(format!("{}: the two ASTs differ", path.display()));
            }
        }

        assert!(
            mismatches.is_empty(),
            "{} of {compared} corpus file(s) disagree:\n{}",
            mismatches.len(),
            mismatches
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        // A guard against the comparison quietly emptying out — a size cap that
        // excluded everything, or a corpus directory that moved.
        assert!(
            compared >= 200,
            "expected a good many corpus files under the cap; only {compared} were compared"
        );
    });
}
