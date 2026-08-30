use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// `set x = x + 1;` — an increment written the long way; `set x++;` is the
/// form for it. Either operand order counts (`x + 1` and `1 + x` both add
/// one), and `set x++;` itself never trips this: it parses as the *increment*
/// operator (`'P'`), which is a different node entirely.
///
/// Only a bare-identifier LHS is flagged, because that is all `set x++;`
/// accepts — a field store (`set p.n = p.n + 1;`) has no shorter spelling to
/// recommend.
///
/// `incr` is the `++` import to recommend, from [`matching_increment`]: the
/// flavor that adds the way this file's `+` does. `None` when the file
/// already imports it, so the message stays short.
pub(super) fn incr_by_one(e: &Expr, incr: Option<&str>, hits: &mut Vec<Error>) {
    let ExprKind::Assign(lhs, value, _) = &e.kind else {
        return;
    };
    let ExprKind::Ident(name) = &lhs.kind else {
        return;
    };
    let ExprKind::Binop(l, '+', r) = &value.kind else {
        return;
    };
    let is_one = |e: &Expr| matches!(e.kind, ExprKind::Num(1));
    let is_target = |e: &Expr| matches!(&e.kind, ExprKind::Ident(n) if n == name);
    if !((is_target(l) && is_one(r)) || (is_one(l) && is_target(r))) {
        return;
    }
    // Name the import too when it's missing: `++` has no bare form, so
    // following the advice without it only trades this error for the
    // operator gate's.
    let import = incr
        .map(|n| format!(", importing `{n} as ++` from builtins"))
        .unwrap_or_default();
    hits.push(Error::at(
        format!(
            "adding 1 to \"{name}\" — use the increment form \"set {name}++;\"{import} \
             (or append #[allow] to this line to keep it)"
        ),
        e.span.clone(),
    ));
}

/// The `++` flavor that increments the way this file's `+` adds, and whether
/// it still needs importing — `Some(None)` when the file already has it,
/// `Some(Some(name))` when [`incr_by_one`] should name it, and `None` when
/// the lint must stay quiet.
///
/// Both operators are bound by import and neither binding is fixed, so the
/// two spellings only agree when they resolve to the same implementation:
/// `wrapping_add as +` pairs with `wrapping_increment as ++`, and
/// `saturating_add as +` with `saturating_increment as ++` — each pair
/// sharing one `__builtin_*_add` (see `OPERATOR_BUILTINS`). The lint stays
/// quiet when the file's `+` is a user function (no `++` flavor could match
/// it), and when its `++` is already bound to the *other* flavor, where
/// `set x++;` would add differently than the `+ 1` it replaces.
pub(super) fn matching_increment(program: &Program) -> Option<Option<&'static str>> {
    // What each operator's import resolves to: `None` for unbound, and
    // `Some(None)` for a binding that isn't an operator builtin at all (a
    // user function), which no `++` flavor can match.
    let (mut plus, mut incr) = (None, None);
    for item in &program.items {
        let Item::Import(decl) = item else {
            continue;
        };
        let from_builtins = matches!(decl.source, ImportSource::Builtins { .. });
        for n in &decl.names {
            let canonical = from_builtins
                .then(|| crate::operator_builtin(&n.name))
                .flatten()
                .map(|(_, canonical)| canonical);
            match n.local() {
                "+" => plus = Some(canonical),
                "++" => incr = Some(canonical),
                _ => {}
            }
        }
    }
    let plus = plus??;
    let name = crate::operator_builtin_named("++", plus)?;
    match incr {
        // Already imported, and it adds the same way: recommend the form alone.
        Some(Some(c)) if c == plus => Some(None),
        // Bound to something else — the other flavor, or a user function.
        Some(_) => None,
        None => Some(Some(name)),
    }
}
