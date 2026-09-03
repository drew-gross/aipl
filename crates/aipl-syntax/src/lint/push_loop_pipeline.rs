use crate::ast::{Expr, ExprKind, Program};
use crate::Error;

use super::{imported_as, lone_stmt, pushed_element, spans_its_text};

/// The local names this file's imports give the three builtins the
/// [`push_loop_pipeline`] rewrite is written in terms of. `None` means the
/// file hasn't imported that one — which for `push` means the lint stays
/// quiet (a `push` it never imported is some other function), and for
/// `map`/`filter` only means the advice has to name the import too, the way
/// [`len_gt_zero`](super::len_gt_zero()) names `is_nonempty`.
///
/// Only a builtins import counts, and each is matched on its *exported*
/// name so an alias (`filter as keep`) is followed rather than missed.
pub(super) struct PipelineNames {
    push: Option<String>,
    map: Option<String>,
    filter: Option<String>,
}

pub(super) fn pipeline_names(program: &Program) -> PipelineNames {
    PipelineNames {
        push: imported_as(program, "push"),
        map: imported_as(program, "map"),
        filter: imported_as(program, "filter"),
    }
}

/// Whether `e` can be lifted into the pipeline's lambda: it must be
/// [`lambda_safe`](super::lambda_safe()), and it may not mention `acc` — the
/// array being accumulated — because the rewritten pipeline no longer has it
/// (`out.len()` as a running index is the shape this catches).
fn liftable(e: &Expr, acc: &str) -> bool {
    if !super::lambda_safe(e) {
        return false;
    }
    let mut ok = true;
    crate::each_subexpr(e, &mut |x| {
        if matches!(&x.kind, ExprKind::Ident(n) if n == acc) {
            ok = false;
        }
    });
    ok
}

/// ```text
/// mut out = [];
/// for (let x : xs) {
///     if (keep(x)) {
///         set out.push(f(x));
///     };
/// }
/// ```
///
/// — an array seeded empty and filled by a loop that does nothing but test
/// each element and push something derived from it. That is
/// `xs.filter(|x| keep(x)).map(|x| f(x))`: one expression that says the
/// array *is* the selected elements, rather than three statements the reader
/// has to run in their head to find out that nothing else happens to `out`.
///
/// Both halves are optional, and which are present picks the advice: with no
/// guard it is a `map`, with the element pushed unchanged a `filter`, and
/// with neither the loop is copying `xs` element by element and `xs` itself
/// is the answer.
///
/// The shape has to be *exactly* this, because anything else in the loop is
/// something the pipeline would drop: the loop body is one statement (the
/// push, or an else-less `if` around one push), the guard and the pushed
/// element may not mention `out` (see [`liftable`]), and the file's
/// `push` must be the builtin. What comes *after* the loop is unconstrained
/// — a later `set out.push(..)` is fine, since the rewrite leaves `out`
/// a `mut` binding that simply starts out filled.
///
/// The one thing this cannot check is that `xs` is an array: `map` and
/// `filter` take arrays, but a `for` also iterates a `str`'s bytes, and
/// which one this is only becomes known two passes later. A `str` loop that
/// collects into an array is thus the lint's blind spot, and `#[allow]` is
/// the answer there.
pub(super) fn push_loop_pipeline(
    e: &Expr,
    src: &str,
    names: &PipelineNames,
    hits: &mut Vec<Error>,
) {
    let Some(push) = &names.push else {
        return;
    };
    // `mut acc = [];` — the seed. An annotation is allowed (and usual: it is
    // often what gives the empty literal its element type) but says nothing
    // about the iterable, so it is not inspected.
    let ExprKind::LetMut(acc, _, seed, body) = &e.kind else {
        return;
    };
    if !matches!(&seed.kind, ExprKind::ArrayLit(xs) if xs.is_empty()) {
        return;
    }
    // The very next statement must be the loop: a statement between the two
    // could read `acc` while it is still empty, which the rewrite would
    // reorder. (A `for` is folded as `Seq(For, rest)` — see `wrap_stmt`.)
    let ExprKind::Seq(first, _) = &body.kind else {
        return;
    };
    let ExprKind::For(var, iterable, loop_body) = &first.kind else {
        return;
    };
    let Some(stmt) = lone_stmt(loop_body) else {
        return;
    };
    // An else-less `if` around the push is the filter; anything else in the
    // body (an `else`, a second statement) is work the pipeline has nowhere
    // to put.
    let (guard, stmt) = match &stmt.kind {
        ExprKind::If(cond, then, els) if matches!(els.kind, ExprKind::Unit) => {
            let Some(stmt) = lone_stmt(then) else {
                return;
            };
            (Some(&**cond), stmt)
        }
        _ => (None, stmt),
    };
    let Some((elem, _)) = pushed_element(stmt, acc, push) else {
        return;
    };
    if guard.is_some_and(|c| !liftable(c, acc)) || !liftable(elem, acc) {
        return;
    }
    // A push of the loop variable itself is the identity map, which the
    // pipeline just leaves out.
    let maps = !matches!(&elem.kind, ExprKind::Ident(n) if n == var);
    // Quote the iterable back only where its span really covers its text —
    // see [`spans_its_text`]. The alternative, checking the source for the
    // header's own `)`, is exactly the case that fails: a call-shaped span
    // stops *before* its closing paren, so `evens(n)` passes that test and
    // splices back as `evens(n`.
    let recv = spans_its_text(iterable).then(|| &src[iterable.span.clone()]);
    // Name whichever of the two the file hasn't imported: like `++` and
    // `is_nonempty`, a builtin has to be in scope before the advice can be
    // followed, and following it without the import only trades this error
    // for the import gate's.
    let mut missing: Vec<&str> = Vec::new();
    let mut stage = |local: &Option<String>, builtin: &'static str| match local {
        Some(n) => n.clone(),
        None => {
            missing.push(builtin);
            builtin.to_string()
        }
    };
    let mut chain = String::new();
    if guard.is_some() {
        let name = stage(&names.filter, "filter");
        chain.push_str(&format!(".{name}(|{var}| ..)"));
    }
    if maps {
        let name = stage(&names.map, "map");
        chain.push_str(&format!(".{name}(|{var}| ..)"));
    }
    let import = if missing.is_empty() {
        String::new()
    } else {
        format!(", importing `{}` from builtins", missing.join("` and `"))
    };
    // A quotable iterable is spliced in; anything else leaves a placeholder,
    // the way `slice_from_zero` writes `[..end]`.
    let recv = recv.unwrap_or("<iterable>");
    let advice = if chain.is_empty() {
        // Neither half: the loop copies the iterable one element at a time.
        format!("it is \"{recv}\" already — assign that and drop the loop")
    } else {
        format!("write \"mut {acc} = {recv}{chain};\" and drop the loop")
    };
    // Point at the seed, not the loop. `#[allow]` is line-scoped (see
    // `allow_squelch`), so a hit spanning the loop could never be squelched:
    // the marker would have to go on the `for (..) {` header, and `aipl fmt`
    // relocates one written there onto a line of its own, where it squelches
    // nothing. `mut {acc} = [];` is a short statement that always occupies
    // one line, and it is where the shape starts.
    hits.push(Error::at(
        format!(
            "\"{acc}\" is seeded empty and filled by this loop and nothing else — \
             {advice}{import} (or append #[allow] to this line to keep it)"
        ),
        seed.span.clone(),
    ));
}
