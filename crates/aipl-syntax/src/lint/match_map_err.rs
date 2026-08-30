use crate::ast::{Expr, ExprKind, Pattern};
use crate::Error;

/// `match (r) { ok(v) => ok(v), err(e) => err(g) }` — a result whose error
/// side is rebuilt and whose value passes straight through. That is exactly
/// `r.map_err(|e| g)`.
///
/// The `ok` arm handing its own binding back to `ok` is what makes this a
/// `map_err` and not a `match` that transforms both sides. Arm order doesn't
/// matter.
///
/// Unlike [`match_value_or`](super::match_value_or()), there is no eager/lazy question to guard
/// against and so no restriction on what the `err` arm builds: `map_err`
/// takes a *function*, which runs on the error path exactly when the arm
/// did. The one shape left alone is `err(e) => err(e)` — a `match` that
/// rebuilds its scrutinee unchanged is a no-op, and `map_err(|e| e)` would
/// be a longer way to write the same nothing.
pub(super) fn match_map_err(e: &Expr, hits: &mut Vec<Error>) {
    let ExprKind::Match(scrut, arms) = &e.kind else {
        return;
    };
    if arms.len() != 2 {
        return;
    }
    let arm_named = |want: &str| {
        arms.iter()
            .find(|a| matches!(&a.pattern, Pattern::Ctor { name, .. } if name == want))
    };
    let (Some(ok_arm), Some(err_arm)) = (arm_named("ok"), arm_named("err")) else {
        return;
    };
    let Pattern::Ctor {
        bindings: ok_binds, ..
    } = &ok_arm.pattern
    else {
        return;
    };
    let Pattern::Ctor {
        bindings: err_binds,
        ..
    } = &err_arm.pattern
    else {
        return;
    };
    // Each side binds exactly its payload. A unit-ok result (`ok => ok`)
    // binds nothing and has no payload to pass through, so it isn't this.
    let ([ok_binder], [err_binder]) = (&ok_binds[..], &err_binds[..]) else {
        return;
    };
    // The identity arm: `ok(v) => ok(v)`, the same binding straight back.
    let ExprKind::Call(ok_name, ok_args, _) = &ok_arm.body.kind else {
        return;
    };
    let [ok_arg] = &ok_args[..] else {
        return;
    };
    if ok_name != "ok" || !matches!(&ok_arg.kind, ExprKind::Ident(v) if v == ok_binder) {
        return;
    }
    // The error arm must rebuild an `err`; anything else is a `match` that
    // leaves the result world, which `map_err` cannot express.
    let ExprKind::Call(err_name, err_args, _) = &err_arm.body.kind else {
        return;
    };
    let [err_arg] = &err_args[..] else {
        return;
    };
    if err_name != "err" {
        return;
    }
    // `err(e) => err(e)` — see the doc comment: a no-op, left alone.
    if matches!(&err_arg.kind, ExprKind::Ident(v) if v == err_binder) {
        return;
    }
    // Quote the scrutinee back only when it is a bare name, whose span is
    // exactly its text — a call-shaped scrutinee's span stops before its
    // parens (see `slice_from_zero`), so splicing it would produce advice
    // that doesn't parse.
    let call = match &scrut.kind {
        ExprKind::Ident(name) => format!("{name}.map_err(|{err_binder}| ..)"),
        _ => format!(".map_err(|{err_binder}| ..)"),
    };
    // Point at the `ok(v) => ok(v)` arm's body, not the whole `match`.
    // `#[allow]` is line-scoped (see `allow_squelch`), so a hit spanning the
    // multi-line `match` could never be squelched. That pass-through is
    // always one line — the `err` arm's replacement need not be — and it is
    // the arm that identifies the shape.
    hits.push(Error::at(
        format!(
            "this `match` rewrites the result's error side and passes its value \
             through — write \"{call}\" instead (or append #[allow] to this line \
             to keep it)"
        ),
        ok_arm.body.span.clone(),
    ));
}
