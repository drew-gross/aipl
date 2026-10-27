use crate::ast::{Expr, ExprKind, Pattern, Program};
use crate::Error;

use super::{imported_as, liftable, lone_stmt, pushed_element, spans_its_text};

/// The local names this file's imports give the builtins the
/// [`push_loop_collect_ok`] rewrite is written in terms of. `push` missing means
/// the lint stays quiet (a `push` it never imported is some other function);
/// `map` and `collect_ok` only mean the advice has to name the import too.
pub(super) struct CollectOkNames {
    push: Option<String>,
    map: Option<String>,
    collect_ok: Option<String>,
}

pub(super) fn collect_ok_names(program: &Program) -> CollectOkNames {
    CollectOkNames {
        push: imported_as(program, "push"),
        map: imported_as(program, "map"),
        collect_ok: imported_as(program, "collect_ok"),
    }
}

/// ```text
/// mut out = [];
/// for (let x : xs) {
///     if (let some(v) = f(x)) {
///         set out.push(g(v));
///     };
/// }
/// ```
///
/// — an array seeded empty and filled by a loop that asks something of each
/// element and pushes only the answers that exist. That is
/// `xs.map(|x| f(x)).collect_ok()` (no `map` first when the loop variable is the
/// optional itself), with a `.map(|v| g(v))` after it when what
/// is pushed is more than the bound name: one expression that says the array *is*
/// the answers, rather than a loop the reader has to run to find out that nothing
/// else happens to `out`.
///
/// It is the optional-guard sibling of `push_loop_pipeline`, which reads a
/// boolean guard and so a `filter`; the two are disjoint by the kind of `if`.
/// The shape is held to the same discipline: the loop body is the one `if let`
/// around the one push, it has no `else`, the pattern is a plain `some(v)`, and
/// neither the scrutinee nor the pushed element may mention `out` (see
/// [`liftable`]). An indexed loop is left alone, since `map` has no index to
/// pass, and a `str` or set receiver is the blind spot `push_loop_pipeline`
/// documents: `#[allow]` is the answer there.
pub(super) fn push_loop_collect_ok(
    e: &Expr,
    src: &str,
    names: &CollectOkNames,
    hits: &mut Vec<Error>,
) {
    let Some(push) = &names.push else {
        return;
    };
    let ExprKind::LetMut(acc, _, seed, body) = &e.kind else {
        return;
    };
    if !matches!(&seed.kind, ExprKind::ArrayLit(xs) if xs.is_empty()) {
        return;
    }
    let ExprKind::Seq(first, _) = &body.kind else {
        return;
    };
    let ExprKind::For(var, None, iterable, loop_body) = &first.kind else {
        return;
    };
    let is_range = matches!(&iterable.kind, ExprKind::Construct(n, _) if n == "__builtin_Span");
    let Some(stmt) = lone_stmt(loop_body) else {
        return;
    };
    let ExprKind::IfLet(arm, scrutinee, otherwise) = &stmt.kind else {
        return;
    };
    if !matches!(otherwise.kind, ExprKind::Unit) {
        return;
    }
    let Pattern::Ctor {
        name,
        bindings,
        ignore_payload: false,
    } = &arm.pattern
    else {
        return;
    };
    let [bound] = bindings.as_slice() else {
        return;
    };
    if name != "some" || bound == "_" {
        return;
    }
    let Some(pushed) = lone_stmt(&arm.body) else {
        return;
    };
    let Some((elem, _)) = pushed_element(pushed, acc, push) else {
        return;
    };
    if !liftable(scrutinee, acc) || !liftable(elem, acc) {
        return;
    }
    // Pushing the bound name itself needs no second `map`.
    let maps = !matches!(&elem.kind, ExprKind::Ident(n) if n == bound);
    let recv = spans_its_text(iterable).then(|| {
        let text = &src[iterable.span.clone()];
        if is_range {
            format!("({text})")
        } else {
            text.to_string()
        }
    });
    let mut missing: Vec<&str> = Vec::new();
    let mut stage = |local: &Option<String>, builtin: &'static str| match local {
        Some(n) => n.clone(),
        None => {
            if !missing.contains(&builtin) {
                missing.push(builtin);
            }
            builtin.to_string()
        }
    };
    // Asking nothing of the element — the iterable already holds the optionals —
    // leaves no first `map` to write.
    let asks = !matches!(&scrutinee.kind, ExprKind::Ident(n) if n == var);
    let mut chain = String::new();
    if asks {
        chain.push_str(&format!(".{}(|{var}| ..)", stage(&names.map, "map")));
    }
    chain.push_str(&format!(".{}()", stage(&names.collect_ok, "collect_ok")));
    if maps {
        chain.push_str(&format!(".{}(|{bound}| ..)", stage(&names.map, "map")));
    }
    let import = if missing.is_empty() {
        String::new()
    } else {
        format!(", importing `{}` from builtins", missing.join("` and `"))
    };
    let recv = recv
        .as_deref()
        .unwrap_or(if is_range { "(<range>)" } else { "<iterable>" });
    // Point at the seed, like `push_loop_pipeline`: `#[allow]` is line-scoped and
    // the seed is a short statement that always occupies one line.
    hits.push(Error::at(
        format!(
            "\"{acc}\" is seeded empty and filled by this loop with the answers that exist \
             and nothing else — write \"mut {acc} = {recv}{chain};\" and drop the loop{import}"
        ),
        seed.span.clone(),
    ));
}
