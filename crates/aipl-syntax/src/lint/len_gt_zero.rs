use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// What [`len_gt_zero`] needs from the file's imports: whether `<` and `>`
/// are the comparisons they look like, and the local name `is_nonempty`
/// goes by here (`None` when the file hasn't imported it yet).
pub(super) struct LenZeroCmp {
    lt: bool,
    gt: bool,
    nonempty: Option<String>,
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
        nonempty: None,
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
                // Aliasable like any builtin, and the advice below has to
                // spell whatever this file calls it.
                _ if n.name == "is_nonempty" => cmp.nonempty = Some(n.local().to_string()),
                _ => {}
            }
        }
    }
    cmp
}

/// `0 < x.len()` / `x.len() > 0` — a length computed only to be compared
/// against zero. `x.is_nonempty()` asks the question directly: it says what
/// the test means rather than leaving the reader to recognize the idiom, and
/// it is the one spelling that reads the same for every receiver `len`
/// takes.
///
/// Only these two orders qualify. `0 > x.len()` and `x.len() < 0` are
/// something else entirely (both are always false on an unsigned length),
/// and the `!= 0` spellings are left alone: this lint's job is the pair of
/// idioms people actually reach for, not every expression that happens to
/// mention a length and a zero.
pub(super) fn len_gt_zero(e: &Expr, src: &str, cmp: &LenZeroCmp, hits: &mut Vec<Error>) {
    let ExprKind::Binop(l, op, r) = &e.kind else {
        return;
    };
    let is_zero = |e: &Expr| matches!(e.kind, ExprKind::Num(0));
    let recv = match op {
        '<' if cmp.lt && is_zero(l) => len_receiver(r),
        '>' if cmp.gt && is_zero(r) => len_receiver(l),
        _ => None,
    };
    let Some(recv) = recv else {
        return;
    };
    let name = cmp.nonempty.as_deref().unwrap_or("is_nonempty");
    // Name the import too when it's missing — like `++`, the builtin has to
    // be imported before the advice can be followed.
    let import = match cmp.nonempty {
        Some(_) => String::new(),
        None => format!(", importing `{name}` from builtins"),
    };
    // Quote the receiver only where the rewrite is a literal splice — the
    // method spelling, whose `.len` follows the receiver's span verbatim.
    // That check is what keeps a parenthesized or free-call receiver
    // (`len(f(x))`, `(a + b).len()`) from being spliced into advice that
    // would reassociate it.
    let advice = if src[recv.span.end..].starts_with(".len") {
        format!("use \"{}.{name}()\"", &src[recv.span.clone()])
    } else {
        format!("use \"{name}\" on the receiver")
    };
    hits.push(Error::at(
        format!(
            "a length compared against 0 — {advice}{import} (or append #[allow] to this line \
             to keep it)"
        ),
        e.span.clone(),
    ));
}

/// The receiver of a `len` call, however it was spelled (`x.len()` is stored
/// as the free call `len(x)`), or `None` when `e` is not one.
fn len_receiver(e: &Expr) -> Option<&Expr> {
    let ExprKind::Call(name, args, _) = &e.kind else {
        return None;
    };
    (name == "len" && args.len() == 1).then(|| &args[0])
}
