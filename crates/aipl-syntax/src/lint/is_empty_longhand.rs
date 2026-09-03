use crate::ast::{BinOp, Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// What [`is_empty_longhand`] needs from the file's imports: whether `==` and `!`
/// are what they look like, and the local names `is_nonempty` and `is_empty` go
/// by here.
pub(super) struct EmptyNames {
    eq: bool,
    not: bool,
    nonempty: Option<String>,
    empty: Option<String>,
    /// What this file's builtins `len` is called, or `None` when `len` here is
    /// a function of the file's own — see `builtin_len_name`.
    len: Option<String>,
}

/// Read those out of the import list. Operators are bound by import and nothing
/// fixes that binding, so a file that aliases `==` or `!` to a function of its
/// own has an `x.len() == 0` that is not an equality test at all — the lint stays
/// quiet there, exactly as [`len_gt_zero`](super::len_gt_zero()) bows out when
/// `<`/`>` are not comparisons. Only a builtins import counts.
pub(super) fn empty_names(program: &Program) -> EmptyNames {
    let mut names = EmptyNames {
        eq: false,
        not: false,
        nonempty: None,
        empty: None,
        len: super::builtin_len_name(program),
    };
    for item in &program.items {
        let Item::Import(decl) = item else {
            continue;
        };
        if !matches!(decl.source, ImportSource::Builtins { .. }) {
            continue;
        }
        for n in &decl.names {
            // A single-semantics operator's canonical impl is the operator
            // spelling itself (see `OPERATOR_BUILTINS`), so the operator this
            // name provides is all there is to check.
            let op = crate::operator_builtin(&n.name).map(|(op, _)| op);
            match n.local() {
                "==" => names.eq = op == Some("=="),
                "!" => names.not = op == Some("!"),
                _ if n.name == "is_nonempty" => names.nonempty = Some(n.local().to_string()),
                _ if n.name == "is_empty" => names.empty = Some(n.local().to_string()),
                _ => {}
            }
        }
    }
    names
}

/// `!x.is_nonempty()` and `x.len() == 0` — emptiness asked the long way.
/// `x.is_empty()` asks it directly.
///
/// The two spellings fail differently and are both worth replacing. The negated
/// predicate makes a reader hold a double negative to see what is being tested;
/// the length comparison computes a number nobody wants and leaves them to
/// recognize the idiom. `is_empty` says the thing itself, and reads the same for
/// every receiver `len` takes — array, `str`, set, dict.
///
/// This is the mirror of [`len_gt_zero`](super::len_gt_zero()), which sends the
/// opposite comparison to `is_nonempty`. Between them, a length is never
/// compared against zero.
///
/// `!= 0` is deliberately not flagged: it is the *non*-empty test spelled with a
/// comparison, so its rewrite is `is_nonempty` rather than `is_empty`, and that
/// belongs to the sibling lint if anywhere. Nor is `x.len() < 1` — true of the
/// same values, but it is not an idiom anyone reaches for, and flagging it would
/// mean guessing at what an unusual spelling meant.
pub(super) fn is_empty_longhand(e: &Expr, src: &str, names: &EmptyNames, hits: &mut Vec<Error>) {
    let recv = match &e.kind {
        // `!x.is_nonempty()`
        ExprKind::Not(inner) if names.not => match &inner.kind {
            ExprKind::Call(f, args, _)
                if args.len() == 1 && names.nonempty.as_deref().is_some_and(|n| n == f) =>
            {
                Some(&args[0])
            }
            _ => None,
        },
        // `x.len() == 0`, either operand order.
        ExprKind::Binop(l, BinOp::Eq, r) if names.eq => {
            let is_zero = |e: &Expr| matches!(e.kind, ExprKind::Num(0));
            let Some(len) = names.len.as_deref() else {
                return;
            };
            if is_zero(r) {
                len_receiver(l, len)
            } else if is_zero(l) {
                len_receiver(r, len)
            } else {
                None
            }
        }
        _ => None,
    };
    let Some(recv) = recv else {
        return;
    };
    let name = names.empty.as_deref().unwrap_or("is_empty");
    // Name the import too when it's missing: following the advice without it
    // only trades this error for the unimported-name one.
    let import = match names.empty {
        Some(_) => String::new(),
        None => format!(", importing `{name}` from builtins"),
    };
    // Quote the receiver only where the rewrite is a literal splice. A call's
    // span stops before its closing paren, so splicing `len(f(x))`'s receiver
    // would produce advice that doesn't parse.
    let advice = if crate::lint::spans_its_text(recv) {
        format!("use \"{}.{name}()\"", &src[recv.span.clone()])
    } else {
        format!("use \"{name}\" on the receiver")
    };
    hits.push(Error::at(
        format!(
            "emptiness asked the long way — {advice}{import} (or append #[allow] to this \
             line to keep it)"
        ),
        e.span.clone(),
    ));
}

/// The receiver of a `len` call, however it was spelled (`x.len()` is stored as
/// the free call `len(x)`), or `None` when `e` is not one.
fn len_receiver<'a>(e: &'a Expr, len: &str) -> Option<&'a Expr> {
    let ExprKind::Call(name, args, _) = &e.kind else {
        return None;
    };
    (name == len && args.len() == 1).then(|| &args[0])
}
