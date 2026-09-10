use crate::ast::{Expr, ExprKind, Program};
use crate::Error;

use super::{imported_as, liftable, lone_stmt, spans_its_text};

/// The local names this file's imports give the three builtins the
/// [`union_loop_union_all`] rewrite is written in terms of. `union` is the
/// precondition — a file that never imported it cannot be writing a set union,
/// and some other `union` is not this lint's business — while `map` and
/// `union_all` only decide whether the advice has to name an import too, the way
/// [`push_loop_pipeline`](super::push_loop_pipeline()) does.
pub(super) struct UnionNames {
    union: Option<String>,
    map: Option<String>,
    union_all: Option<String>,
}

pub(super) fn union_names(program: &Program) -> UnionNames {
    UnionNames {
        union: imported_as(program, "union"),
        map: imported_as(program, "map"),
        union_all: imported_as(program, "union_all"),
    }
}

/// The expression unioned into `acc` by `value`, when `value` is a union of
/// `acc` with exactly one other thing.
///
/// Both operand orders count. Set union is commutative *and* its result is a
/// set, so `acc.union(x)` and `x.union(acc)` accumulate the same set — unlike
/// `concat`, where the order is the whole answer. Refusing the flipped spelling
/// would only mean the lint stays quiet on a fold that is just as foldable.
fn unioned_operand<'a>(value: &'a Expr, acc: &str, union: &str) -> Option<&'a Expr> {
    let ExprKind::Call(name, args, _) = &value.kind else {
        return None;
    };
    if name != union || args.len() != 2 {
        return None;
    }
    let is_acc = |e: &Expr| matches!(&e.kind, ExprKind::Ident(n) if n == acc);
    match (is_acc(&args[0]), is_acc(&args[1])) {
        (true, false) => Some(&args[1]),
        (false, true) => Some(&args[0]),
        // `acc.union(acc)` is `acc`, and a union of two other things is not a
        // fold onto this binding at all.
        _ => None,
    }
}

/// ```text
/// mut out = #{};
/// for (let s : xs) {
///     set out = out.union(s);
/// }
/// ```
///
/// — a set seeded empty and grown by a loop that does nothing but union each
/// element into it. That is `xs.union_all()`: one word for "every element of
/// every set in `xs`", instead of three statements the reader has to run in
/// their head to discover that the loop is a plain fold and nothing else
/// happens to `out`.
///
/// When the loop unions something *derived* from the element rather than the
/// element itself, that derivation is the `map` the pipeline needs first:
/// `set out = out.union(f(s));` is `xs.map(|s| f(s)).union_all()`. This is the
/// same two-stage advice [`push_loop_pipeline`](super::push_loop_pipeline())
/// gives, and it bows out in the same places — the unioned expression may not
/// mention `out` (a fold reading its own accumulator is not a `map`) and may not
/// propagate with `?` (see [`lambda_safe`](super::lambda_safe())).
///
/// The shape has to be exactly this, because anything else in the loop is work
/// `union_all` has nowhere to put: the body is one statement, that statement is
/// the assignment, and the seed is the *empty* set — `mut out = #{1};` folds
/// onto a starting value the builtin cannot express. A guard is refused for the
/// same reason, even though `xs.filter(..).union_all()` would say it: the guard
/// would have to be a predicate on whole sets, which is a different and much
/// rarer thing to write than a predicate on elements, and advising it on the
/// strength of a shape match is how a lint starts being wrong. What comes
/// *after* the loop is unconstrained, as in the push lints — the rewrite leaves
/// `out` a `mut` binding that simply starts out full.
///
/// An early `return` inside the loop takes the shape out of scope
/// automatically: the body is then two statements, so
/// [`lone_stmt`](super::lone_stmt()) declines. That is `seq_labels` in
/// `crates/aipl-codegen/src/parse.aipl`, which stops at the first non-nullable
/// element and is a genuinely different fold.
pub(super) fn union_loop_union_all(e: &Expr, src: &str, names: &UnionNames, hits: &mut Vec<Error>) {
    let Some(union) = &names.union else {
        return;
    };
    // `mut acc = #{};` — the seed. An annotation is allowed (and usual: for a
    // set it is often the only thing that gives the empty literal its element
    // type) but says nothing about the iterable, so it is not inspected.
    let ExprKind::LetMut(acc, _, seed, body) = &e.kind else {
        return;
    };
    if !matches!(&seed.kind, ExprKind::SetLit(xs) if xs.is_empty()) {
        return;
    }
    // The very next statement must be the loop: a statement between the two
    // could read `acc` while it is still empty, which the rewrite would
    // reorder. (A `for` is folded as `Seq(For, rest)` — see `wrap_stmt`.)
    let ExprKind::Seq(first, _) = &body.kind else {
        return;
    };
    let ExprKind::For(var, iterable, loop_body) = &first.kind else {
        return;
    };
    let Some(stmt) = lone_stmt(loop_body) else {
        return;
    };
    let ExprKind::Assign(lhs, value, _) = &stmt.kind else {
        return;
    };
    if !matches!(&lhs.kind, ExprKind::Ident(n) if n == acc) {
        return;
    }
    let Some(operand) = unioned_operand(value, acc, union) else {
        return;
    };
    if !liftable(operand, acc) {
        return;
    }

    // Unioning the loop variable itself is the identity map, which the pipeline
    // just leaves out.
    let maps = !matches!(&operand.kind, ExprKind::Ident(n) if n == var);
    // Quote the iterable back only where its span really covers its text — see
    // [`spans_its_text`]. A call-shaped span stops before its closing paren, so
    // splicing one back would produce source that doesn't parse.
    let recv = spans_its_text(iterable).then(|| &src[iterable.span.clone()]);
    // Name whichever builtin the file hasn't imported: the advice cannot be
    // followed until it is in scope, and following it without the import only
    // trades this error for the import gate's.
    let mut missing: Vec<&str> = Vec::new();
    let mut stage = |local: &Option<String>, builtin: &'static str| match local {
        Some(n) => n.clone(),
        None => {
            missing.push(builtin);
            builtin.to_string()
        }
    };
    let mut chain = String::new();
    if maps {
        let name = stage(&names.map, "map");
        chain.push_str(&format!(".{name}(|{var}| ..)"));
    }
    let fold = stage(&names.union_all, "union_all");
    chain.push_str(&format!(".{fold}()"));
    let import = if missing.is_empty() {
        String::new()
    } else {
        format!(", importing `{}` from builtins", missing.join("` and `"))
    };
    let recv = recv.unwrap_or("<iterable>");
    // Point at the seed, not the loop. `#[allow]` is line-scoped (see
    // `allow_squelch`), so a hit spanning the loop could never be squelched: the
    // marker would have to go on the `for (..) {` header, and `aipl fmt`
    // relocates one written there onto a line of its own, where it squelches
    // nothing. `mut {acc} = #{};` is a short statement that always occupies one
    // line, and it is where the shape starts.
    hits.push(Error::at(
        format!(
            "\"{acc}\" is seeded empty and unioned into by this loop and nothing else — \
             write \"mut {acc} = {recv}{chain};\" and drop the loop\
             {import} (or append #[allow] to this line to keep it)"
        ),
        seed.span.clone(),
    ));
}
