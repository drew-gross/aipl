use crate::ast::{Expr, ExprKind};
use crate::Error;

/// `Point { x: x }` — a field initialized with the bare identifier of the
/// same name; recommend the shorthand `Point { x }`. The shorthand desugars
/// to exactly this AST, so the two forms are indistinguishable by shape
/// alone: the explicit form is identified from source by a `:` immediately
/// before the value (the shorthand's value span *is* the field name, with no
/// colon before it). A comment between the `:` and the value leaves it
/// un-flagged — the conservative choice that never mis-flags a shorthand.
pub(super) fn field_init_shorthand(e: &Expr, src: &str, hits: &mut Vec<Error>) {
    let ExprKind::Construct(name, inits) = &e.kind else {
        return;
    };
    // Synthetic constructs the parser desugars from other syntax carry a
    // `__builtin_`-prefixed name a user can't write — e.g. a range
    // `start..end` becomes `__builtin_Span { start: start, end: end }`. The
    // bound identifiers aren't a written `field: field`, so skip these (the
    // source-scan below would otherwise read the *outer* field's colon).
    if name.starts_with("__builtin_") {
        return;
    }
    for init in inits {
        let ExprKind::Ident(n) = &init.value.kind else {
            continue;
        };
        if n != &init.name {
            continue;
        }
        if !src[..init.value.span.start].trim_end().ends_with(':') {
            continue; // already the shorthand form
        }
        hits.push(Error::at(
            format!(
                "field \"{n}\" is set to the identifier of the same name — use the \
                 shorthand \"{n}\" (or append #[allow] to this line to keep it)"
            ),
            init.value.span.clone(),
        ));
    }
}
