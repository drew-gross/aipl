use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

use super::{imported_as, lone_stmt, spans_its_text};

/// What [`return_loop_any_all`] needs from the file's imports: the local names
/// `any` and `all` go by here, and whether `!` is really negation.
pub(super) struct AnyAllNames {
    any: Option<String>,
    all: Option<String>,
    /// Whether this file's `!` is `logical_not`. A file may alias `!` to a
    /// function of its own, and then `!pred(x)` is an ordinary call whose
    /// meaning the rewrite must not assume — the same reason
    /// [`is_empty_longhand`](super::is_empty_longhand()) checks it.
    not: bool,
}

/// Read those out of the import list. `any` and `all` are ordinary builtins
/// rather than operators, so a file that has not imported one is told to.
pub(super) fn any_all_names(program: &Program) -> AnyAllNames {
    let mut not = false;
    for item in &program.items {
        let Item::Import(decl) = item else {
            continue;
        };
        if !matches!(decl.source, ImportSource::Builtins { .. }) {
            continue;
        }
        for n in &decl.names {
            if n.local() == "!" {
                not = crate::operator_builtin(&n.name).map(|(op, _)| op) == Some("!");
            }
        }
    }
    AnyAllNames {
        any: imported_as(program, "any"),
        all: imported_as(program, "all"),
        not,
    }
}

/// ```text
/// for (let x : xs) {
///     if (matches(x)) {
///         return true;
///     };
/// }
/// false
/// ```
///
/// — a loop that reports whether *some* element passes a test. That is
/// `xs.any(|x| matches(x))`. The mirror image, returning `false` from the loop
/// and falling through to `true`, asks whether *every* element passes: with the
/// condition negated (`if (!matches(x))`) it is `xs.all(|x| matches(x))`.
///
/// Both spellings make the reader run the loop in their head to discover that
/// the early return is the only way out and that the literal after the loop is
/// the answer for "no element did" — which is the whole content of `any` and
/// `all`, said in one word.
///
/// The shape has to be exactly this, for the reasons `return_loop_find_if`
/// spells out: the body is an else-less `if` around a lone `return` of a bool
/// literal, the loop is immediately followed by the *opposite* literal, and a
/// condition that propagates with `?` is left alone
/// ([`lambda_safe`](super::lambda_safe())). A loop returning the same literal it
/// falls through to is not a fold of anything and is left alone too.
///
/// **The `all` direction fires only on a syntactically negated condition.**
/// `if (x == 0) { return false; }` is `all(|x| x != 0)`, which needs the
/// comparison inverted — and inverting an arbitrary condition correctly is not
/// something to do in a diagnostic. Advising `!xs.any(..)` instead would be
/// right but worse than what it replaces. So that shape is left alone; only the
/// `!pred(x)` form, where the negation lifts off, is advised.
pub(super) fn return_loop_any_all(e: &Expr, src: &str, names: &AnyAllNames, hits: &mut Vec<Error>) {
    // A `for` is folded as `Seq(For, rest)` (see `wrap_stmt`), and `rest` is
    // what the loop falls through to.
    let ExprKind::Seq(first, rest) = &e.kind else {
        return;
    };
    let ExprKind::For(var, iterable, loop_body) = &first.kind else {
        return;
    };
    let Some(fell_through) = bool_answer(rest) else {
        return;
    };
    // An else-less `if` around the return: an `else`, or a second statement, is
    // work neither builtin has anywhere to put.
    let Some(stmt) = lone_stmt(loop_body) else {
        return;
    };
    let ExprKind::If(cond, then, els) = &stmt.kind else {
        return;
    };
    if !matches!(els.kind, ExprKind::Unit) {
        return;
    }
    let Some(ret) = lone_stmt(then) else {
        return;
    };
    let ExprKind::Return(value) = &ret.kind else {
        return;
    };
    let ExprKind::Bool(returned) = value.kind else {
        return;
    };
    // The two literals must disagree. `return true` above a `true` tail answers
    // the same thing either way and folds to a constant, not to `any`.
    if returned == fell_through {
        return;
    }
    if !super::lambda_safe(cond) {
        return;
    }

    // `return true` / `false` is `any` of the condition as written; `return
    // false` / `true` is `all` of it with the negation lifted off.
    let builtin = if returned {
        "any"
    } else {
        // Only the `!pred(x)` form — see the doc comment on why an arbitrary
        // condition is left alone rather than inverted in a diagnostic.
        if strip_not(cond, names.not).is_none() {
            return;
        }
        "all"
    };
    let local = if builtin == "any" {
        &names.any
    } else {
        &names.all
    };
    let (name, import) = match local {
        Some(local) => (local.as_str(), String::new()),
        None => (builtin, format!(", importing `{builtin}` from builtins")),
    };

    // Quote the iterable back only where its span really covers its text — see
    // [`spans_its_text`](super::spans_its_text()) — and otherwise the
    // placeholder the sibling lints use.
    let recv = spans_its_text(iterable)
        .then(|| &src[iterable.span.clone()])
        .unwrap_or("<iterable>");
    // The `all` rewrite takes the condition with its `!` removed, which is the
    // one part of the advice a reader could get backwards — so it is said rather
    // than left to the `..` placeholder the sibling lints use for a predicate.
    let (what, note) = if builtin == "any" {
        ("whether any element passes a test", "")
    } else {
        (
            "whether every element passes a test",
            " (the condition with its \"!\" removed)",
        )
    };
    // Point at the `return`, not the loop. `#[allow]` is line-scoped, so a hit
    // spanning the loop could never be squelched: the marker would have to go on
    // the `for (..) {` header, and `aipl fmt` relocates one written there onto a
    // line of its own, where it squelches nothing. `return true;` is a short
    // statement that always occupies one line, and it is the line that makes
    // this a fold to a single answer.
    hits.push(Error::at(
        format!(
            "this loop reports {what} — write \"return {recv}.{name}(|{var}| ..);\"{note} and \
             drop the loop{import} (or append #[allow] to this line to keep it)"
        ),
        ret.span.clone(),
    ));
}

/// The bool literal `rest` — what the loop falls through to — answers with: a
/// bare `true`/`false` as the block's tail, or an explicit `return true;`.
/// Anything else, and the fall-through does work neither builtin would do.
fn bool_answer(rest: &Expr) -> Option<bool> {
    match &rest.kind {
        ExprKind::Bool(b) => Some(*b),
        // `return false;` folds as `Seq(Return(false), <unreachable rest>)`.
        ExprKind::Seq(stmt, _) => match &stmt.kind {
            ExprKind::Return(v) => match v.kind {
                ExprKind::Bool(b) => Some(b),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// `x` from `!x`, when this file's `!` is really `logical_not`. `None` for
/// anything else, which is what keeps the `all` advice from having to invert a
/// condition it does not understand.
fn strip_not(cond: &Expr, not_is_logical: bool) -> Option<&Expr> {
    if !not_is_logical {
        return None;
    }
    match &cond.kind {
        ExprKind::Call(op, args, _) if op == "!" && args.len() == 1 => Some(&args[0]),
        _ => None,
    }
}
