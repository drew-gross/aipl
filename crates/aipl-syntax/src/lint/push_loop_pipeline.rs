use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

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
    let mut names = PipelineNames {
        push: None,
        map: None,
        filter: None,
    };
    for item in &program.items {
        let Item::Import(decl) = item else {
            continue;
        };
        if !matches!(decl.source, ImportSource::Builtins { .. }) {
            continue;
        }
        for n in &decl.names {
            let local = || Some(n.local().to_string());
            match n.name.as_str() {
                "push" => names.push = local(),
                "map" => names.map = local(),
                "filter" => names.filter = local(),
                _ => {}
            }
        }
    }
    names
}

/// Whether `e` can be moved into a lambda unchanged.
///
/// Two things stop it. Mentioning `acc` — the array being accumulated —
/// means the expression reads a value the rewritten pipeline no longer has
/// (`out.len()` as a running index is the shape this catches). And a `?`
/// propagation returns from the function it is written in, so relocating it
/// into a lambda would return from the *lambda* instead: same source, a
/// different early exit.
fn lambda_safe(e: &Expr, acc: &str) -> bool {
    let mut ok = true;
    crate::each_subexpr(e, &mut |x| match &x.kind {
        ExprKind::Ident(n) if n == acc => ok = false,
        ExprKind::Try(_) => ok = false,
        _ => {}
    });
    ok
}

/// The synthetic value the parser ends a statement-only block with: `()` for
/// an ordinary block, and an i64 `0` — the loop expression's own result —
/// for a loop body, which may not end in a value of its own at all. The
/// loop's zero is told from a written one by its empty span, the same way
/// [`slice_from_zero`](super::slice_from_zero()) tells an elided slice bound from a typed one.
fn is_block_tail(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Unit => true,
        ExprKind::Num(0) => e.span.is_empty(),
        _ => false,
    }
}

/// The single statement a block consists of, or `None` when it holds
/// anything else — no statements, or more than one.
///
/// A block is a right-nested chain, and which node carries the rest of it
/// depends on the statement: an expression statement is a [`ExprKind::Seq`]
/// whose second operand is the remainder, while `let`/`mut`/`set` each hold
/// their own. Only the two forms a push loop can be made of are recognized
/// here, and each is "lone" exactly when what follows it is the block's
/// tail (see [`is_block_tail`]).
fn lone_stmt(block: &Expr) -> Option<&Expr> {
    match &block.kind {
        ExprKind::Seq(stmt, rest) if is_block_tail(rest) => Some(stmt),
        ExprKind::Assign(_, _, rest) if is_block_tail(rest) => Some(block),
        _ => None,
    }
}

/// `set acc.push(elem);` (or its longhand `set acc = acc.push(elem);`) —
/// the pushed element, when `stmt` is a push onto `acc` and nothing else.
/// `push` is this file's local name for the builtin, so an aliased import is
/// followed and a *user* function that happens to be called `push` is not.
fn pushed_element<'a>(stmt: &'a Expr, acc: &str, push: &str) -> Option<&'a Expr> {
    let ExprKind::Assign(lhs, value, _) = &stmt.kind else {
        return None;
    };
    if !matches!(&lhs.kind, ExprKind::Ident(n) if n == acc) {
        return None;
    }
    let ExprKind::Call(name, args, _) = &value.kind else {
        return None;
    };
    if name != push || args.len() != 2 {
        return None;
    }
    if !matches!(&args[0].kind, ExprKind::Ident(n) if n == acc) {
        return None;
    }
    Some(&args[1])
}

/// Whether `e`'s span covers exactly the text that spells it, so the source
/// can be spliced back into advice. Only a name and a field path off one
/// qualify: their spans run from the first character to the last. Most other
/// forms do not — a call's span stops before its closing paren, so `f(x)`
/// splices back as `f(x`, which doesn't parse — and the ones that might are
/// left out rather than each having to be re-checked whenever a span moves.
fn spans_its_text(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Ident(_) => true,
        ExprKind::Field(recv, _) => spans_its_text(recv),
        _ => false,
    }
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
/// element may not mention `out` (see [`lambda_safe`]), and the file's
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
    let Some(elem) = pushed_element(stmt, acc, push) else {
        return;
    };
    if guard.is_some_and(|c| !lambda_safe(c, acc)) || !lambda_safe(elem, acc) {
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
