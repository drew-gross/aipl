use crate::ast::{Expr, ExprKind, Program};
use crate::Error;

/// `` `..{to_str(x)}..` `` — a `to_str` inside a template interpolation. The
/// interpolation *is* `to_str`: a template lowers each `{e}` to
/// `__template_interp(e)`, which renders `e` exactly as `to_str` does (a `str`
/// as its text, a `char` as its character, anything else by its type), so the
/// call converts a value to the string the interpolation was about to make of
/// it anyway. `{x}` says the same thing.
///
/// Only the file's builtins `to_str` counts, by whatever local name it goes
/// by, and only as the interpolation's whole expression: `{to_str(x) +++ y}`
/// is not this shape, and a `to_str` of the file's own would be some other
/// function.
pub(super) fn interp_to_str(e: &Expr, src: &str, to_str: &str, hits: &mut Vec<Error>) {
    let ExprKind::Call(name, args, _) = &e.kind else {
        return;
    };
    if name != "__template_interp" {
        return;
    }
    let [inner] = args.as_slice() else {
        return;
    };
    let ExprKind::Call(callee, cargs, _) = &inner.kind else {
        return;
    };
    if callee != to_str || cargs.len() != 1 {
        return;
    }
    // Splice the argument into the advice only where its span is its text —
    // a call's span stops before its closing paren (`spans_its_text`).
    let advice = if crate::lint::spans_its_text(&cargs[0]) {
        format!("write \"{{{}}}\"", &src[cargs[0].span.clone()])
    } else {
        "interpolate the value itself".to_string()
    };
    hits.push(Error::at(
        format!(
            "\"{to_str}\" inside an interpolation is redundant — the interpolation already \
             renders its value, so {advice} (or append #[allow] to this line to keep it)"
        ),
        inner.span.clone(),
    ));
}

/// The local name this file's builtins `to_str` goes by, or `None` when it is
/// not imported — in which case no call here is the builtin.
pub(super) fn to_str_name(program: &Program) -> Option<String> {
    crate::lint::imported_as(program, "to_str")
}
