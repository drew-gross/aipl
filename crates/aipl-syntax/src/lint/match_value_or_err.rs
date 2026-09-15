use crate::ast::{Callee, Expr, ExprKind, Pattern};
use crate::Error;

/// Whether the optimizer will sink `e` into the branch that uses it, rather than
/// evaluating it where it was written.
///
/// This is the lint's local stand-in for `aipl_mono::sink::can_defer`, which it
/// cannot call: that runs after monomorphization over a call-graph closure,
/// while a lint runs per file, before type checking. The two must agree on what
/// is deferrable, because the whole justification for advising this rewrite is
/// that the sinker undoes its eagerness.
///
/// The structural half is the same: nothing that leaves the function from where
/// it sits, writes a binding, or may not terminate. Arithmetic is absent because
/// none of it aborts — `/` and `%` are both total (see `saturating_rem` in
/// codegen), so there is no longer any expression whose eager evaluation could
/// kill a program that used to return.
///
/// The *effect* half is where the two differ, and where AIPL's effect discipline
/// does the work: a caller must declare at least the effects of everything it
/// calls, so a function that declares none cannot reach an effectful call at
/// all. `pure_fn` is that fact about the enclosing function, and it makes the
/// question answerable without a call graph. In an effect-declaring function the
/// lint simply stays quiet, since it cannot tell which calls carry the effect.
fn sinkable(e: &Expr, pure_fn: bool) -> bool {
    fn blocks(e: &Expr) -> bool {
        matches!(
            &e.kind,
            // Leaves the function from where it sits.
            ExprKind::Try(_) | ExprKind::Return(_)
            // May write a binding declared outside the value.
            | ExprKind::Assign(..)
            // May not terminate.
            | ExprKind::For(..) | ExprKind::While(..)
        )
            // `assert` aborts, and that is its whole purpose.
            || matches!(&e.kind, ExprKind::Call(n, _, _) if n == "assert" || *n == Callee::Assert)
            || crate::children(e).iter().any(|c| blocks(c))
    }
    pure_fn && !blocks(e)
}

/// `match (o) { some(v) => ok(v), none => err(e) }` — an optional turned into a
/// result by naming both cases. That is exactly `o.value_or_err(e)`.
///
/// The `none` arm building an `err` is what identifies the shape: it is the
/// bridge from the optional world to the result world, and `value_or_err` is
/// the name of that bridge. Arm order doesn't matter.
///
/// The `some` arm is *not* constrained to the identity `ok(v)`, because the
/// rewrite does not need it to be. `some(v) => ok(f(v))` is
/// `ok(f(o.value_or_err(e)?))` — the same `value_or_err`, with `?` doing the
/// early return the `none` arm was doing by hand. The advice says which of the
/// two it is, since the second reads quite differently from the first.
///
/// The `none` arm may also *leave* with the error — a block holding nothing
/// but `return err(e);` — which is the statement form of the same bridge: the
/// `match` runs for effect, the `some` arm's block is the rest of the function,
/// and the `return` stands in for the `?`. That one rewrites to
/// `let v = o.value_or_err(e)?;` followed by the `some` arm's body, with `v`
/// the arm's binder, and the advice spells it that way.
///
/// A *computed* error is fine, and that is the point of [`sinkable`]. Written
/// out, `value_or_err`'s error is an ordinary call argument and so looks eager —
/// but the optimizer inlines the builtin and sinks the argument into the `none`
/// branch, so it is built exactly when the `match` built it. Measured on a
/// struct error interpolating a runtime string: both spellings allocate the
/// same, to the byte.
///
/// This is where the rule differs from [`match_value_or`](super::match_value_or()),
/// whose `constant_value` guard predates that reasoning. The lint stays quiet
/// only for an error the sinker genuinely cannot defer — one that aborts, writes,
/// or may not terminate, or that sits in an effect-declaring function where its
/// effects cannot be told apart.
pub(super) fn match_value_or_err(e: &Expr, src: &str, pure_fn: bool, hits: &mut Vec<Error>) {
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
    let (Some(some_arm), Some(none_arm)) = (arm_named("some"), arm_named("none")) else {
        return;
    };
    let Pattern::Ctor { bindings, .. } = &some_arm.pattern else {
        return;
    };
    let Pattern::Ctor {
        bindings: none_binds,
        ..
    } = &none_arm.pattern
    else {
        return;
    };
    // `some` binds exactly its payload; `none` binds nothing.
    let [binder] = &bindings[..] else {
        return;
    };
    if !none_binds.is_empty() {
        return;
    }
    // The `none` arm must build an `err` from one value — as its value, or
    // as what it returns. Anything else is a `match` that does something this
    // rule has no rewrite for.
    let Some((error, leaves)) = none_err(&none_arm.body) else {
        return;
    };
    if !sinkable(error, pure_fn) {
        return;
    }
    // Whether the `some` arm is the identity `ok(v)` — the shape that rewrites
    // to a bare `value_or_err`, with no `?` and nothing left around it.
    let passes_through = match &some_arm.body.kind {
        ExprKind::Call(name, args, _) => match &args[..] {
            [arg] => *name == Callee::Ok && matches!(&arg.kind, ExprKind::Ident(v) if v == binder),
            _ => false,
        },
        _ => false,
    };
    // Quote the scrutinee back only when it is a bare name, whose span is
    // exactly its text — a call-shaped scrutinee's span stops before its parens
    // (see `slice_from_zero`), so splicing it would produce advice that doesn't
    // parse.
    let receiver = match &scrut.kind {
        ExprKind::Ident(name) => name.as_str(),
        _ => "",
    };
    // Same rule for the error value, and for the same reason `match_value_or`
    // applies it to its default: a single token splices back verbatim, escapes
    // and quotes included, while anything wider has a span that stops at its
    // last token — before a construction's closing brace — so quoting it would
    // print advice that doesn't parse. Then the message names it instead.
    let quoted: Option<&str> = matches!(
        error.kind,
        ExprKind::Num(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Ident(_)
    )
    .then(|| src.get(error.span.start..error.span.end))
    .flatten();
    let advice = match (passes_through, quoted) {
        // The statement form: the `some` arm's block continues with the
        // payload bound, so the rewrite is a `let` ahead of that block.
        _ if leaves.is_some() => match quoted {
            Some(q) => format!(
                "write \"let {binder} = {receiver}.value_or_err({q})?;\" and continue with the \
                 `some` arm's body — `?` does the early return this arm is doing by hand"
            ),
            None => format!(
                "write it as \"let {binder} = {receiver}.value_or_err(..)?;\" with that arm's \
                 error and continue with the `some` arm's body — `?` does the early return \
                 this arm is doing by hand"
            ),
        },
        (true, Some(q)) => format!("write \"{receiver}.value_or_err({q})\" instead"),
        (true, None) => {
            format!("write it as \"{receiver}.value_or_err(..)\" with that arm's error")
        }
        (false, Some(q)) => format!(
            "write \"{receiver}.value_or_err({q})?\" and use the payload directly — `?` does \
             the early return this arm is doing by hand"
        ),
        (false, None) => format!(
            "write it as \"{receiver}.value_or_err(..)?\" with that arm's error, and use the \
             payload directly — `?` does the early return this arm is doing by hand"
        ),
    };
    // Point at the `none => err(..)` arm's body, not the whole `match`.
    // `#[allow]` is line-scoped (see `check`), so a hit spanning the multi-line
    // `match` could never be squelched. This arm is the one that identifies the
    // shape — the `some` arm varies — and its `err` call always starts on the
    // arm's own line, which therefore takes a trailing marker and keeps it
    // through `aipl fmt`. For the block form that line is the `return`'s: a
    // marker after the arm's `{` would not survive formatting.
    let at = leaves.unwrap_or(&none_arm.body);
    hits.push(Error::at(
        format!(
            "this `match` turns an optional into a result — {advice} \
             (or append #[allow] to this line to keep it)"
        ),
        at.span.clone(),
    ));
}

/// The error a `none` arm's `body` builds, and — when the arm leaves the
/// function with it rather than yielding it — the `return` doing so, which is
/// where the hit is reported. `None` for a body of any other shape.
///
/// Two spellings: `err(e)`, and a block holding nothing but `return err(e);`
/// (a bare `return` is not an arm body — an arm takes an expression or a
/// block). A block with anything else in it is an arm that does more than
/// build the error, which `value_or_err` has nowhere to put.
fn none_err(body: &Expr) -> Option<(&Expr, Option<&Expr>)> {
    fn err_payload(e: &Expr) -> Option<&Expr> {
        match &e.kind {
            ExprKind::Call(Callee::Err, args, _) => match &args[..] {
                [error] => Some(error),
                _ => None,
            },
            _ => None,
        }
    }
    if let Some(error) = err_payload(body) {
        return Some((error, None));
    }
    let stmt = super::lone_stmt(body)?;
    let ExprKind::Return(value) = &stmt.kind else {
        return None;
    };
    Some((err_payload(value)?, Some(stmt)))
}
