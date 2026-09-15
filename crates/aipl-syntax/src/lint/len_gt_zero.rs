use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// What [`len_gt_zero`] needs from the file's imports: whether `<`, `>` and
/// `!` are the operators they look like, the local name `is_nonempty` goes by
/// here (`None` when the file hasn't imported it yet), and the name
/// `is_empty` goes by, for the negated spelling.
pub(super) struct LenZeroCmp {
    lt: bool,
    gt: bool,
    not: bool,
    nonempty: Option<String>,
    empty: Option<String>,
    /// What this file's builtins `len` is called, or `None` when `len` here is a
    /// function of the file's own — see `builtin_len_name`.
    len: Option<String>,
}

/// Read those three names out of the import list. Operators are bound by
/// import and nothing fixes that binding, so a file that aliases `<` to a
/// function of its own has a `0 < x.len()` that is not a length test at all
/// — the lint stays quiet there, the way [`incr_by_one`](super::incr_by_one()) bows out when a
/// file's `++` and `+` disagree. Only a builtins import counts: the operator
/// builtins are the ones whose meaning is fixed.
pub(super) fn len_zero_cmp(program: &Program) -> LenZeroCmp {
    let mut cmp = LenZeroCmp {
        lt: false,
        gt: false,
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
                "<" => cmp.lt = op == Some("<"),
                ">" => cmp.gt = op == Some(">"),
                "!" => cmp.not = op == Some("!"),
                // Aliasable like any builtin, and the advice below has to
                // spell whatever this file calls it.
                _ if n.name == "is_nonempty" => cmp.nonempty = Some(n.local().to_string()),
                _ if n.name == "is_empty" => cmp.empty = Some(n.local().to_string()),
                _ => {}
            }
        }
    }
    cmp
}

/// `0 < x.len()` / `x.len() > 0`, and `!x.is_empty()` — non-emptiness asked
/// the long way. `x.is_nonempty()` asks the question directly: it says what
/// the test means rather than leaving the reader to recognize the idiom (or
/// to hold a negation), and it is the one spelling that reads the same for
/// every receiver `len` takes.
///
/// Only these shapes qualify. `0 > x.len()` and `x.len() < 0` are something
/// else entirely (both are always false on an unsigned length), and the `!=
/// 0` spellings are left alone: this lint's job is the idioms people actually
/// reach for, not every expression that happens to mention a length and a
/// zero. The negated predicate is the mirror of
/// [`is_empty_longhand`](super::is_empty_longhand())'s `!x.is_nonempty()`;
/// between the two, a predicate is never negated to ask the other one.
pub(super) fn len_gt_zero(e: &Expr, src: &str, cmp: &LenZeroCmp, hits: &mut Vec<Error>) {
    let ExprKind::Call(op, args, _) = &e.kind else {
        return;
    };
    let (recv, what) = match args.as_slice() {
        // `!x.is_empty()` — `!` is a call named for its spelling, like every
        // operator, and `is_empty` is whatever this file imported it as.
        [inner] if op == "!" && cmp.not => match &inner.kind {
            ExprKind::Call(f, args, _)
                if args.len() == 1 && cmp.empty.as_deref().is_some_and(|n| n == f) =>
            {
                (&args[0], "a negated emptiness test")
            }
            _ => return,
        },
        [l, r] => {
            let is_zero = |e: &Expr| matches!(e.kind, ExprKind::Num(0));
            // A `len` of the file's own is a different function answering a
            // different question, and `is_nonempty` does not apply to what it
            // takes.
            let Some(len) = cmp.len.as_deref() else {
                return;
            };
            let recv = match op.as_str() {
                "<" if cmp.lt && is_zero(l) => len_receiver(r, len),
                ">" if cmp.gt && is_zero(r) => len_receiver(l, len),
                _ => None,
            };
            let Some(recv) = recv else {
                return;
            };
            (recv, "a length compared against 0")
        }
        _ => return,
    };
    let name = cmp.nonempty.as_deref().unwrap_or("is_nonempty");
    // Name the import too when it's missing — like `++`, the builtin has to
    // be imported before the advice can be followed.
    let import = match cmp.nonempty {
        Some(_) => String::new(),
        None => format!(", importing `{name}` from builtins"),
    };
    // Quote the receiver only where the rewrite is a literal splice — the
    // method spelling, whose `.len`/`.is_empty` follows the receiver's span
    // verbatim. That check is what keeps a parenthesized or free-call receiver
    // (`len(f(x))`, `(a + b).len()`) from being spliced into advice that
    // would reassociate it.
    let after = &src[recv.span.end..];
    let empty_method = cmp.empty.as_deref().map(|n| format!(".{n}"));
    let advice = if after.starts_with(".len")
        || empty_method
            .as_deref()
            .is_some_and(|m| after.starts_with(m))
    {
        format!("use \"{}.{name}()\"", &src[recv.span.clone()])
    } else {
        format!("use \"{name}\" on the receiver")
    };
    hits.push(Error::at(
        format!("{what} — {advice}{import} (or append #[allow] to this line to keep it)"),
        e.span.clone(),
    ));
}

/// The receiver of a `len` call, however it was spelled (`x.len()` is stored
/// as the free call `len(x)`), or `None` when `e` is not one.
fn len_receiver<'a>(e: &'a Expr, len: &str) -> Option<&'a Expr> {
    let ExprKind::Call(name, args, _) = &e.kind else {
        return None;
    };
    (name == len && args.len() == 1).then(|| &args[0])
}
