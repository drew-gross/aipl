use crate::ast::{Expr, ExprKind, Program};
use crate::Error;

use super::imported_as;

/// The local names this file's imports give the builtins the
/// [`map_find_if_some`] shape is made of, and the one its advice names. `map`
/// and `find_if` are the precondition — the shape is theirs — and `is_some` is
/// what the predicate has to be; `find_map` only decides whether the advice
/// names an import too.
pub(super) struct FindMapNames {
    map: Option<String>,
    find_if: Option<String>,
    is_some: Option<String>,
    find_map: Option<String>,
}

pub(super) fn find_map_names(program: &Program) -> FindMapNames {
    FindMapNames {
        map: imported_as(program, "map"),
        find_if: imported_as(program, "find_if"),
        is_some: imported_as(program, "is_some"),
        find_map: imported_as(program, "find_map"),
    }
}

/// `xs.map(f).find_if(is_some)` — the first element `f` has an answer for,
/// written as a chain that hands back the answer wrapped twice. `xs.find_map(f)`
/// says it in one word, and hands back the answer once: the chain's result is
/// a `U??` whose outer layer every caller then peels (`.value_or(none)`), while
/// `find_map` returns the `U?` that was wanted.
///
/// The predicate has to *be* `is_some` — the name, or a lambda that only
/// forwards to it — since that is what makes the chain a `find_map` rather than
/// a search over mapped values (`map_find_if`'s shape, which the fusion pass
/// takes care of).
pub(super) fn map_find_if_some(e: &Expr, names: &FindMapNames, hits: &mut Vec<Error>) {
    let (Some(map), Some(find_if), Some(is_some)) = (&names.map, &names.find_if, &names.is_some)
    else {
        return;
    };
    let ExprKind::Call(outer, args, _) = &e.kind else {
        return;
    };
    if outer != find_if || args.len() != 2 {
        return;
    }
    let ExprKind::Call(inner, inner_args, _) = &args[0].kind else {
        return;
    };
    if inner != map || inner_args.len() != 2 {
        return;
    }
    if !is_is_some(&args[1], is_some) {
        return;
    }
    let (find_map, import) = match &names.find_map {
        Some(local) => (local.as_str(), String::new()),
        None => (
            "find_map",
            ", importing `find_map` from builtins".to_string(),
        ),
    };
    hits.push(Error::at(
        format!(
            "\"{map}\" then \"{find_if}({is_some})\" is the first element with an answer, \
             wrapped twice — write \"{find_map}(..)\" over the mapping function, which hands \
             the answer back once{import} (or append #[allow] to this line to keep it)"
        ),
        e.span.clone(),
    ));
}

/// Whether `pred` is `is_some`: the name itself, or `|x| x.is_some()` /
/// `|x| is_some(x)`.
fn is_is_some(pred: &Expr, is_some: &str) -> bool {
    match &pred.kind {
        ExprKind::Ident(n) => n == is_some,
        ExprKind::Lambda(params, body) => {
            let [param] = params.as_slice() else {
                return false;
            };
            matches!(&body.kind, ExprKind::Call(f, args, _)
                if f == is_some
                    && matches!(args.as_slice(), [a] if matches!(&a.kind, ExprKind::Ident(n) if n == &param.name)))
        }
        _ => false,
    }
}
