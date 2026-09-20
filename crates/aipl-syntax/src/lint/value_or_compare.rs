use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// What [`value_or_compare`] needs from the file's imports: whether `==` and
/// `!=` are the comparisons they look like, and what this file calls
/// `value_or` (`None` when it never imported the builtin — a `value_or` of
/// the file's own is not the one this lint knows the meaning of).
pub(super) struct ValueOrCompare {
    eq: bool,
    ne: bool,
    value_or: Option<String>,
}

/// Read those names out of the import list. Only a builtins import counts, for
/// the same reason [`len_zero_cmp`](super::len_zero_cmp()) gives: an operator's
/// meaning is fixed by its import and nothing else, so `==` aliased to a
/// function of the file's own is not an equality test.
pub(super) fn value_or_compare_names(program: &Program) -> ValueOrCompare {
    let mut names = ValueOrCompare {
        eq: false,
        ne: false,
        value_or: None,
    };
    for item in &program.items {
        let Item::Import(decl) = item else {
            continue;
        };
        if !matches!(decl.source, ImportSource::Builtins { .. }) {
            continue;
        }
        for n in &decl.names {
            let op = crate::operator_builtin(&n.name).map(|(op, _)| op);
            match n.local() {
                "==" => names.eq = op == Some("=="),
                "!=" => names.ne = op == Some("!="),
                _ if n.name == "value_or" => names.value_or = Some(n.local().to_string()),
                _ => {}
            }
        }
    }
    names
}

/// `x.value_or(' ') == '.'` — an optional compared against a literal by
/// first filling its `none` with a *second* literal, chosen only so the
/// comparison rejects it. `x == some('.')` asks the question directly: `none`
/// fails the comparison on its own, so there is no made-up value to invent, and
/// no reader has to confirm that the two literals differ. Same for `!=`.
///
/// Only the literal-against-literal shape qualifies, and only when the two
/// literals differ: `x.value_or(0) == 0` is a genuine "none or zero" test that
/// `x == some(0)` would answer differently, and a non-literal on either side
/// could coincide with the default at run time the same way. Both operand
/// orders are matched (`'.' == x.value_or(' ')`), since the rewrite reads the
/// same either way.
pub(super) fn value_or_compare(e: &Expr, src: &str, names: &ValueOrCompare, hits: &mut Vec<Error>) {
    let Some(value_or) = &names.value_or else {
        return;
    };
    let ExprKind::Call(op, args, _) = &e.kind else {
        return;
    };
    let cmp = op.name();
    let known = match cmp {
        "==" => names.eq,
        "!=" => names.ne,
        _ => false,
    };
    if !known {
        return;
    }
    let [l, r] = args.as_slice() else {
        return;
    };
    // Whichever side is the `value_or` call; the other must be a literal.
    let (filled, lit) = match (value_or_of(l, value_or), value_or_of(r, value_or)) {
        (Some(f), None) if is_literal(r) => (f, r),
        (None, Some(f)) if is_literal(l) => (f, l),
        _ => return,
    };
    let (opt, default) = filled;
    if !is_literal(default) || default.kind == lit.kind {
        return;
    }
    let opt_src = &src[opt.span.clone()];
    let lit_src = &src[lit.span.clone()];
    hits.push(Error::at(
        format!(
            "`{value_or}` here only supplies a value for the comparison to reject; compare \
             the optional directly: `{opt_src} {cmp} some({lit_src})` (or append #[allow] to \
             this line to keep it)"
        ),
        default.span.clone(),
    ));
}

/// `(optional, default)` when `e` is a call to this file's `value_or`.
fn value_or_of<'a>(e: &'a Expr, value_or: &str) -> Option<(&'a Expr, &'a Expr)> {
    let ExprKind::Call(callee, args, _) = &e.kind else {
        return None;
    };
    if callee.name() != value_or {
        return None;
    }
    match args.as_slice() {
        [opt, default] => Some((opt, default)),
        _ => None,
    }
}

fn is_literal(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Str(_) | ExprKind::Char(_)
    )
}
