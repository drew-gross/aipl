//! Constructor-equality unwrapping: comparing against `some(e)` / `ok(e)` /
//! `err(e)` without building the wrapper.
//!
//! `x == some(y)` reads as "is `x` this optional", and that is how it compiles:
//! an optional is materialized out of `y` — a stack slot, a tag store, a payload
//! store, and for a heap payload a retain — and then compared field by field
//! against `x`. But the constructed value is dead the instant the comparison
//! ends, and half of it is a tag the comparison already knows. So the whole
//! construction is waste:
//!
//! ```text
//! x == some(y)    →    match (x) {
//!                          some(v) => v == y,
//!                          none => false,
//!                      }
//! ```
//!
//! The rewrite asks the tag directly and compares payloads, which is what the
//! generated `__eq_<n>` helper would have done anyway — minus the value it had
//! to be handed. Measured on a bare `i64?`, the cheapest payload there is: 274
//! instructions down to 211, and 1728 bytes of binary down to 1600. A heap
//! payload saves its retain/release on top.
//!
//! Both operand orders are handled, and `!=` as well as `==` — with the arms'
//! answers inverted, since "not equal" is true exactly where "equal" is false.
//!
//! # Why the operand keeps its place
//!
//! The comparison's other operand is *not* moved into the arm. Sinking it there
//! would mean not evaluating it when the tag misses, which is only sound if
//! evaluating it cannot be observed — and `x == some(f())` where `f` prints must
//! print whether or not `x` is `none`.
//!
//! Rather than decide that here, the operand is bound ahead of the match:
//!
//! ```text
//! x == some(f())  →   let t = f();
//!                     match (x) { some(v) => v == t, none => false }
//! ```
//!
//! which leaves exactly the shape [`crate::sink_bindings`] exists to
//! recognize — a binding whose one user is a single arm of the match right after
//! it. That pass already knows what may be deferred (no effects, no abort, no
//! control flow, no assignment) and it runs later in the pipeline, so a pure
//! operand ends up in the arm anyway and an effectful one stays put. This pass
//! makes the construction go away; that one decides what may follow it in.
//!
//! An operand that is a name or a literal skips the binding — evaluating one is
//! nothing, and the extra `let` would only be something for a later pass to
//! undo.

use aipl_syntax::ast::{Expr, ExprKind, Item, MatchArm, Pattern, Program};

/// The single-payload constructors this rewrites, each with the arm that
/// answers for the *other* case. A result has two cases, so which one carries
/// the payload decides which arm gets the comparison.
fn other_case(ctor: &str) -> Option<&'static str> {
    match ctor {
        "some" => Some("none"),
        "ok" => Some("err"),
        "err" => Some("ok"),
        _ => None,
    }
}

/// Rewrite every `x == some(e)`-shaped comparison in `program` so the wrapper is
/// never constructed. See the module docs.
pub fn unwrap_ctor_eq(program: &Program) -> Program {
    let mut n = 0usize;
    Program {
        // Rewrites bodies only; the file map carries through unchanged.
        sources: program.sources.clone(),
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) => {
                    let mut f = f.clone();
                    f.body = rewrite(&f.body, &mut n);
                    f.test_body = f.test_body.as_ref().map(|x| rewrite(x, &mut n));
                    Item::Fn(f)
                }
                other => other.clone(),
            })
            .collect(),
    }
}

/// Rebuild `e` with its children rewritten first, then rewrite `e` itself.
/// Bottom-up, so a comparison uncovered by rewriting a child is still seen.
fn rewrite(e: &Expr, n: &mut usize) -> Expr {
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        let done = rewrite(child, n);
        *child = done;
    }
    rewrite_here(out, n)
}

/// One rewrite step at `e` itself, or `e` unchanged when the shape doesn't hold.
fn rewrite_here(e: Expr, n: &mut usize) -> Expr {
    let ExprKind::Call(cmp, args, method) = &e.kind else {
        return e;
    };
    let eq = match cmp.as_str() {
        "__builtin_equal" => true,
        "__builtin_not_equal" => false,
        _ => return e,
    };
    if args.len() != 2 {
        return e;
    }
    // Exactly one side must be the construction. Both sides being one is a
    // comparison of two literals, which constant folding is the pass for; the
    // rewrite would only turn it into a match on a value that is right there.
    let (scrut, ctor_call) = match (ctor_payload(&args[0]), ctor_payload(&args[1])) {
        (None, Some(c)) => (&args[0], c),
        (Some(c), None) => (&args[1], c),
        _ => return e,
    };
    let (ctor, payload) = ctor_call;
    let Some(miss_case) = other_case(ctor) else {
        return e;
    };

    *n += 1;
    let bound = format!("__ceq{n}");
    // A name or a literal is free to evaluate, so it goes straight into the arm;
    // anything else is bound ahead of the match for `sink_bindings` to place.
    let trivial = matches!(
        &payload.kind,
        ExprKind::Ident(_)
            | ExprKind::Num(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
    );
    let operand = if trivial {
        payload.clone()
    } else {
        Expr::rebuilt(ExprKind::Ident(bound.clone()), payload)
    };

    let payload_var = format!("__ceqv{n}");
    let compare = Expr::rebuilt(
        ExprKind::Call(
            cmp.clone(),
            vec![
                Expr::rebuilt(ExprKind::Ident(payload_var.clone()), &e),
                operand,
            ],
            *method,
        ),
        &e,
    );
    let miss = Expr::rebuilt(ExprKind::Bool(!eq), &e);

    let hit_arm = MatchArm {
        pattern: Pattern::Ctor {
            name: ctor.to_string(),
            bindings: vec![payload_var],
            ignore_payload: false,
        },
        body: compare,
        span: e.span.clone(),
    };
    // `none` carries nothing to ignore; `ok`/`err` do, and the arm reads neither.
    let miss_arm = MatchArm {
        pattern: Pattern::Ctor {
            name: miss_case.to_string(),
            bindings: Vec::new(),
            ignore_payload: miss_case != "none",
        },
        body: miss,
        span: e.span.clone(),
    };
    // Arm order follows the scrutinee's cases, so a result reads `ok` then
    // `err` whichever side carried the payload.
    let arms = if ctor == "err" {
        vec![miss_arm, hit_arm]
    } else {
        vec![hit_arm, miss_arm]
    };
    let matched = Expr::rebuilt(ExprKind::Match(Box::new(scrut.clone()), arms), &e);
    if trivial {
        return matched;
    }
    Expr::rebuilt(
        ExprKind::Let(bound, None, Box::new(payload.clone()), Box::new(matched)),
        &e,
    )
}

/// `e` as a single-payload constructor call — `some(x)`, `ok(x)`, `err(x)` —
/// with the constructor's name and what it wraps.
fn ctor_payload(e: &Expr) -> Option<(&str, &Expr)> {
    let ExprKind::Call(name, args, _) = &e.kind else {
        return None;
    };
    if args.len() != 1 || other_case(name).is_none() {
        return None;
    }
    Some((name.as_str(), &args[0]))
}
