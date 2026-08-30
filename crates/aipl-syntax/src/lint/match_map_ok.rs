use crate::ast::{Expr, ExprKind, Pattern};
use crate::Error;

/// `match (r) { ok(v) => ok(g), err(e) => err(e) }` — a result whose value
/// side is rebuilt and whose error passes straight through. That is exactly
/// `r.map_ok(|v| g)`, and the mirror of [`match_map_err`](super::match_map_err()).
///
/// The `err` arm handing its own binding back to `err` is what makes this a
/// `map_ok`; the `ok` arm must do something, since `ok(v) => ok(v)` alongside
/// it is a `match` that rebuilds its scrutinee unchanged. Neither lint fires
/// on that no-op: advising `map_ok(|v| v)` would be a longer way to write
/// the same nothing. Arm order doesn't matter.
///
/// As with `map_err` there is no eager/lazy question — `map_ok` takes a
/// function, which runs on the value path exactly when the arm did — and so
/// no restriction on what the `ok` arm builds.
pub(super) fn match_map_ok(e: &Expr, hits: &mut Vec<Error>) {
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
    // Each side binds exactly its payload. A unit-ok result (`ok => ok`) has
    // no value to rewrite, so it isn't this.
    let ([ok_binder], [err_binder]) = (&ok_binds[..], &err_binds[..]) else {
        return;
    };
    // The identity arm: `err(e) => err(e)`, the same binding straight back.
    let ExprKind::Call(err_name, err_args, _) = &err_arm.body.kind else {
        return;
    };
    let [err_arg] = &err_args[..] else {
        return;
    };
    if err_name != "err" || !matches!(&err_arg.kind, ExprKind::Ident(v) if v == err_binder) {
        return;
    }
    // The value arm must rebuild an `ok`; anything else is a `match` that
    // leaves the result world, which `map_ok` cannot express.
    let ExprKind::Call(ok_name, ok_args, _) = &ok_arm.body.kind else {
        return;
    };
    let [ok_arg] = &ok_args[..] else {
        return;
    };
    if ok_name != "ok" {
        return;
    }
    // `ok(v) => ok(v)` — see the doc comment: with both arms identity the
    // whole `match` is a no-op, left alone by this lint and by `map_err`'s.
    if matches!(&ok_arg.kind, ExprKind::Ident(v) if v == ok_binder) {
        return;
    }
    // Quote the scrutinee back only when it is a bare name, whose span is
    // exactly its text — a call-shaped scrutinee's span stops before its
    // parens (see `slice_from_zero`), so splicing it would produce advice
    // that doesn't parse.
    let call = match &scrut.kind {
        ExprKind::Ident(name) => format!("{name}.map_ok(|{ok_binder}| ..)"),
        _ => format!(".map_ok(|{ok_binder}| ..)"),
    };
    // Point at the `err(e) => err(e)` arm's body, not the whole `match`.
    // `#[allow]` is line-scoped (see `allow_squelch`), so a hit spanning the
    // multi-line `match` could never be squelched. That pass-through is
    // always one line — the `ok` arm's replacement need not be — and it is
    // the arm that identifies the shape.
    hits.push(Error::at(
        format!(
            "this `match` rewrites the result's value side and passes its error \
             through — write \"{call}\" instead (or append #[allow] to this line \
             to keep it)"
        ),
        err_arm.body.span.clone(),
    ));
}
