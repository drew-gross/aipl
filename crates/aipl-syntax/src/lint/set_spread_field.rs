use crate::ast::{Expr, ExprKind};
use crate::{each_subexpr, Error};

/// `set p = P { ..p, x: 1 };` — a binding rebuilt from itself only to replace
/// a field. `set p.x = 1;` is the field store spelled as one: it names the
/// one thing that changes, and the struct's name and its other fields never
/// come up. A field path works the same way — `set o.inner = Inner
/// { ..o.inner, n: 2 }` is `set o.inner.n = 2`.
///
/// Only the spread *of the target itself* qualifies: `set acc = First
/// { ..acc.merge(other), nullable: true }` starts from a different value, and
/// the two stores it would take are no shorter. With several fields the
/// rewrite is one store per field, so it is offered only when no replacement
/// reads the target — `set p = P { ..p, x: p.y, y: p.x }` swaps, and
/// `set p.x = p.y; set p.y = p.x;` does not. A single replacement is always
/// safe: the value is computed before the store either way.
pub(super) fn set_spread_field(e: &Expr, src: &str, hits: &mut Vec<Error>) {
    let ExprKind::Assign(lhs, value, _) = &e.kind else {
        return;
    };
    let ExprKind::Construct(_, inits) = &value.kind else {
        return;
    };
    let Some(root) = root_name(lhs) else {
        return;
    };
    let Some((spread, fields)) = inits.split_first() else {
        return;
    };
    let ExprKind::Spread(base) = &spread.value.kind else {
        return;
    };
    if base.kind != lhs.kind || fields.is_empty() {
        return;
    }
    if fields.len() > 1 && fields.iter().any(|f| mentions(&f.value, root)) {
        return;
    }
    let target = &src[lhs.span.clone()];
    let stores = fields
        .iter()
        .map(|f| {
            format!(
                "set {target}.{} = {};",
                f.name,
                super::quote(&f.value, src, "..")
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let (what, advice) = if fields.len() == 1 {
        (format!("replace {:?}", fields[0].name), "assign the field")
    } else {
        ("replace some fields".to_string(), "assign each field")
    };
    hits.push(Error::at(
        format!(
            "this \"set\" rebuilds {target:?} from itself only to {what} — {advice} \
             directly: \"{stores}\" (or append #[allow] to this line to keep it)"
        ),
        spread.value.span.clone(),
    ));
}

/// The binding a `set` target is rooted in: `p` for `p` and for `p.a.b`.
fn root_name(e: &Expr) -> Option<&str> {
    match &e.kind {
        ExprKind::Ident(n) => Some(n),
        ExprKind::Field(recv, _) => root_name(recv),
        _ => None,
    }
}

fn mentions(e: &Expr, name: &str) -> bool {
    let mut found = false;
    each_subexpr(e, &mut |x| {
        if matches!(&x.kind, ExprKind::Ident(n) if n == name) {
            found = true;
        }
    });
    found
}
