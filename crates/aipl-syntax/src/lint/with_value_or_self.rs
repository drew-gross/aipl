use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// What [`with_value_or_self`] needs from the file's imports: what it calls the
/// `with` and `value_or` builtins, and what it calls `try_set` (`None` when it
/// has not imported it, which the advice then says to do).
pub(super) struct WithNames {
    with: String,
    value_or: String,
    try_set: Option<String>,
}

/// Read those names out of the import list; `None` unless the file imports both
/// builtins the shape is made of, since a function of its own named `with` or
/// `value_or` is not the one this lint knows the meaning of.
pub(super) fn with_names(program: &Program) -> Option<WithNames> {
    let imported = |builtin: &str| {
        program.items.iter().find_map(|item| {
            let Item::Import(decl) = item else {
                return None;
            };
            if !matches!(decl.source, ImportSource::Builtins) {
                return None;
            }
            decl.names
                .iter()
                .find(|n| n.name == builtin)
                .map(|n| n.local().to_string())
        })
    };
    Some(WithNames {
        with: imported("with")?,
        value_or: imported("value_or")?,
        try_set: imported("try_set"),
    })
}

/// `xs.with(i, v).value_or(xs)` — a replacement that falls back to the array it
/// was replacing in. That is `xs.try_set(i, v)`, which says it directly: a write
/// past the end does nothing.
///
/// The longhand also reads `xs` twice, and the first read is the one that wants to
/// be a move: `with` writes into an array nobody else holds, and the fallback is
/// someone else holding it. So it can never write in place; `try_set` can.
///
/// Only when the fallback is the *same place* as the receiver — a binding, or a
/// field path off one. A different array (`xs.with(i, v).value_or(ys)`), or an
/// empty one (`.value_or([])`, for an index known to be in range), says something
/// else and is left alone.
pub(super) fn with_value_or_self(e: &Expr, src: &str, names: &WithNames, hits: &mut Vec<Error>) {
    let ExprKind::Call(callee, args, _) = &e.kind else {
        return;
    };
    if callee.name() != names.value_or {
        return;
    }
    let [replaced, default] = args.as_slice() else {
        return;
    };
    let ExprKind::Call(inner, with_args, _) = &replaced.kind else {
        return;
    };
    if inner.name() != names.with {
        return;
    }
    let [receiver, _, _] = with_args.as_slice() else {
        return;
    };
    if !same_place(receiver, default) {
        return;
    }
    let array = super::quote(receiver, src, "<array>");
    let try_set = names.try_set.as_deref().unwrap_or("try_set");
    let import = if names.try_set.is_none() {
        ", importing `try_set` from builtins"
    } else {
        ""
    };
    hits.push(Error::at(
        format!(
            "this `{}` falls back to the array it is replacing in — write \"{array}.{try_set}(..)\" \
             and drop the fallback{import}",
            names.value_or
        ),
        default.span.clone(),
    ));
}

/// Whether `a` and `b` read the same binding, or the same field path off one.
fn same_place(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Ident(x), ExprKind::Ident(y)) => x == y,
        (ExprKind::Field(x, f), ExprKind::Field(y, g)) => f == g && same_place(x, y),
        _ => false,
    }
}
