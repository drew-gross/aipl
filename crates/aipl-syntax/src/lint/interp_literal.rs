use crate::ast::{Callee, Expr, ExprKind};
use crate::Error;

/// `` `a: {'a'}` `` / `` `aa: {"aa"}` `` — a string or char literal as the whole
/// of a template interpolation. The interpolation renders the literal as its
/// text, which is exactly what the surrounding template would have said had
/// the text been written in place: `` `a: a` ``. The braces, the quotes and
/// the render are all spent on nothing.
///
/// Only a literal standing alone is this shape — `{"a" +++ s}` computes
/// something — and only the escaping forms of it: a `"""` literal is raw, and
/// its contents may need escapes a template would decode (`\{`, a backtick),
/// so it is a way of getting verbatim text into a template rather than a
/// detour.
pub(super) fn interp_literal(e: &Expr, src: &str, hits: &mut Vec<Error>) {
    let ExprKind::Call(Callee::TemplateInterp, args, _) = &e.kind else {
        return;
    };
    let [inner] = args.as_slice() else {
        return;
    };
    // The tree does not record which form a string literal was written in;
    // its first characters do.
    let raw = src
        .get(inner.span.start..)
        .is_some_and(|s| s.starts_with("\"\"\""));
    let what = match &inner.kind {
        ExprKind::Str(_) if !raw => "string",
        ExprKind::Char(_) => "char",
        _ => return,
    };
    hits.push(Error::at(
        format!(
            "a {what} literal interpolated into a template is just its text — write it into \
             the template directly (or append #[allow] to this line to keep it)"
        ),
        inner.span.clone(),
    ));
}
