use crate::ast::{Expr, ExprKind, Program};
use crate::Error;

use super::{imported_as, quote};

/// The local names this file's imports give the builtins the
/// [`map_constant_repeat`] shape and its advice are made of. `map` is the
/// precondition — the shape is its — and `repeat` and `len` are what the advice
/// writes, so a missing one is named as an import to add.
pub(super) struct RepeatNames {
    map: Option<String>,
    repeat: Option<String>,
    len: Option<String>,
}

pub(super) fn repeat_names(program: &Program) -> RepeatNames {
    RepeatNames {
        map: imported_as(program, "map"),
        repeat: imported_as(program, "repeat"),
        len: imported_as(program, "len"),
    }
}

/// `xs.map(|x| c)` where `c` is the same value for every element: a function that
/// ignores its argument is a way to write "this many copies of `c`" that builds
/// the copies one at a time and spells the count as a walk over `xs`.
/// `[c].repeat(xs.len())` says it directly — and builds `c` once, then copies it
/// into an exactly-sized result.
///
/// Only a body that is **constant** qualifies: literals, names read as they are,
/// and constructions or literals of those. Anything that calls a function does
/// not, because `repeat` evaluates its element once where `map` evaluated the
/// body per element, and a call may differ between evaluations (or have an
/// effect). A body that mentions its own parameter is not ignoring it. Names are
/// allowed because reading a binding gives the same value each time.
///
/// What the lint cannot see is the receiver's type: it runs before checking, and
/// `opt.map(|_| c)` is the optional's `map`, for which `.len()` and `repeat` are no
/// answer. The advice is worded for a sequence for that reason, and a receiver
/// that is a range is skipped outright, being syntactically what it is. The cost
/// of the remaining case is one `#[allow]` on a legitimate optional `map`.
///
/// The hit points at the lambda, a short span that begins on the line the
/// `#[allow]` would be written on.
pub(super) fn map_constant_repeat(e: &Expr, src: &str, names: &RepeatNames, hits: &mut Vec<Error>) {
    let Some(map) = &names.map else {
        return;
    };
    let ExprKind::Call(callee, args, _) = &e.kind else {
        return;
    };
    if callee != map {
        return;
    }
    let [recv, lambda] = args.as_slice() else {
        return;
    };
    let ExprKind::Lambda(params, body) = &lambda.kind else {
        return;
    };
    let [param] = params.as_slice() else {
        return;
    };
    if is_range(recv) || !is_constant(body, &param.name) {
        return;
    }
    let repeat = names.repeat.as_deref().unwrap_or("repeat");
    let len = names.len.as_deref().unwrap_or("len");
    let missing: Vec<String> = [
        (names.repeat.is_none()).then(|| "`repeat`".to_string()),
        (names.len.is_none()).then(|| "`len`".to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();
    let import = if missing.is_empty() {
        String::new()
    } else {
        format!(", importing {} from builtins", missing.join(" and "))
    };
    let element = quote(body, src, "<element>");
    let items = quote(recv, src, "<items>");
    hits.push(Error::at(
        format!(
            "\"{map}\" with a function that ignores its argument builds the same value once \
             per element — for a sequence, write \"[{element}].{repeat}({items}.{len}())\"{import}"
        ),
        lambda.span.clone(),
    ));
}

/// Whether evaluating `e` gives the same value every time, with no effect: a
/// literal, a name other than `param` (read as it is), or a construction or
/// literal of such. A call is not constant — see [`map_constant_repeat`].
fn is_constant(e: &Expr, param: &str) -> bool {
    match &e.kind {
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => true,
        ExprKind::Ident(name) => name != param,
        ExprKind::Neg(inner) => is_constant(inner, param),
        ExprKind::Construct(_, inits) => inits.iter().all(|i| is_constant(&i.value, param)),
        ExprKind::ArrayLit(elems) | ExprKind::SetLit(elems, _) | ExprKind::TupleLit(elems) => {
            elems.iter().all(|x| is_constant(x, param))
        }
        ExprKind::DictLit(pairs) => pairs
            .iter()
            .all(|(k, v)| is_constant(k, param) && is_constant(v, param)),
        _ => false,
    }
}

/// Whether `e` is a range (`a..b`), which builds the builtin `Span` — the one
/// receiver of `map` that is known from syntax alone not to be an array or an
/// optional.
fn is_range(e: &Expr) -> bool {
    matches!(&e.kind, ExprKind::Construct(name, _) if name.ends_with("Span"))
}
