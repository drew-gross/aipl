use crate::ast::{BinOp, Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// `set x = x + e;` — an accumulate written the long way; `set x += e;` is the
/// form for it, and likewise `-=`, `*=`, `/=`.
///
/// For the commutative operators either operand order counts (`x * k` and
/// `k * x` both scale `x`), but `-` and `/` are flagged only with the receiver
/// on the left: `set x = k - x;` is not `set x -= k;`, and rewriting it that
/// way would silently change the program.
///
/// Only a bare-identifier LHS is flagged — that is all `set x += e;` accepts,
/// for the reason the form exists at all: the receiver is mentioned twice, so a
/// field path or a call would have to be re-evaluated. A field store
/// (`set p.n = p.n + 1;`) therefore has no shorter spelling to recommend.
///
/// `incr` says whether [`super::incr_by_one`] is live for this file. When it
/// is, `set x = x + 1;` belongs to it — `set x++;` is the more specific advice
/// — and this lint bows out rather than offering a second rewrite of the same
/// line. When it isn't (no `++` flavor matches this file's `+`), `+= 1` is the
/// only advice left and this lint gives it.
///
/// `ops` is what to recommend per operator, from [`matching_compound`]: the
/// compound spelling, plus the import to name when the file lacks it.
pub(super) fn compound_assign(e: &Expr, ops: &CompoundOps, incr: bool, hits: &mut Vec<Error>) {
    let ExprKind::Assign(lhs, value, _) = &e.kind else {
        return;
    };
    let ExprKind::Ident(name) = &lhs.kind else {
        return;
    };
    let ExprKind::Binop(l, op, r) = &value.kind else {
        return;
    };
    let Some((spelling, import)) = ops.get(*op) else {
        return;
    };
    let is_target = |e: &Expr| matches!(&e.kind, ExprKind::Ident(n) if n == name);
    // The receiver on the left works for every operator; on the right only
    // where the operation is commutative, since `k - x` and `k / x` are not
    // accumulations of `x` at all.
    let operand = if is_target(l) {
        r
    } else if is_target(r) && matches!(op, BinOp::Add | BinOp::Mul) {
        l
    } else {
        return;
    };
    // `set x = x + 1;` is the increment lint's when that lint can fire here.
    if incr && matches!(op, BinOp::Add) && matches!(operand.kind, ExprKind::Num(1)) {
        return;
    }
    // Name the import too when it's missing: no compound operator has a bare
    // form, so following the advice without it only trades this error for the
    // operator gate's.
    let import = import
        .map(|n| format!(", importing `{n} as {spelling}` from builtins"))
        .unwrap_or_default();
    hits.push(Error::at(
        format!(
            "\"{name}\" accumulates onto itself — use the compound form \
             \"set {name} {spelling} ...;\"{import} (or append #[allow] to this line to keep it)"
        ),
        e.span.clone(),
    ));
}

/// Which compound assignments this file can be advised to use, by the plain
/// operator each replaces: the compound spelling, and the builtin to import
/// when the file doesn't have it yet (`None` when it already does).
///
/// Built by [`matching_compound`]; an operator absent from here is one this
/// lint must stay quiet about.
#[derive(Default)]
pub(super) struct CompoundOps {
    ops: Vec<(BinOp, &'static str, Option<&'static str>)>,
}

impl CompoundOps {
    fn get(&self, op: BinOp) -> Option<(&'static str, Option<&'static str>)> {
        self.ops
            .iter()
            .find(|(o, _, _)| *o == op)
            .map(|(_, spelling, import)| (*spelling, *import))
    }

    /// Nothing to advise anywhere — the driver skips the walk entirely.
    pub(super) fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// The compound form of each plain operator this file imports, where one
/// provably means the same thing.
///
/// Both spellings are bound by import and neither binding is fixed, so — as
/// with `+`/`++` (see [`super::matching_increment`]) — the two only agree when
/// they resolve to the same implementation: `wrapping_add as +` pairs with
/// `wrapping_add_assign as +=`, `saturating_add as +` with
/// `saturating_add_assign as +=`. Where the operator has only *one* flavor
/// (`*`, `/`) there is nothing to match and the sole compound form is it —
/// which is also the only way `/=` can be reached, its canonical being a marker
/// rather than a shared `__builtin_*`.
///
/// An operator is left out when its `+` is a user function (no compound flavor
/// could match it) or when its compound spelling is already bound to something
/// else, where the rewrite would not mean the same thing.
pub(super) fn matching_compound(program: &Program) -> CompoundOps {
    // What each operator spelling's import resolves to: `None` for unbound, and
    // `Some(None)` for a binding that is not an operator builtin at all (a user
    // function), which no compound flavor can match.
    let mut bound: Vec<(&str, Option<&'static str>)> = Vec::new();
    for item in &program.items {
        let Item::Import(decl) = item else {
            continue;
        };
        let from_builtins = matches!(decl.source, ImportSource::Builtins { .. });
        for n in &decl.names {
            let canonical = from_builtins
                .then(|| crate::operator_builtin(&n.name))
                .flatten()
                .map(|(_, canonical)| canonical);
            bound.push((n.local(), canonical));
        }
    }
    let binding = |spelling: &str| bound.iter().find(|(l, _)| *l == spelling).map(|(_, c)| *c);

    let mut ops = Vec::new();
    for (op, plain, compound) in [
        (BinOp::Add, "+", "+="),
        (BinOp::Sub, "-", "-="),
        (BinOp::Mul, "*", "*="),
        (BinOp::Div, "/", "/="),
    ] {
        // The plain operator has to be this file's, and a builtin: a user's `+`
        // has no compound flavor to pair with.
        let Some(Some(plain_impl)) = binding(plain) else {
            continue;
        };
        // Where the compound operator has one flavor there is nothing to
        // choose; where it has several, the one sharing the plain operator's
        // implementation is the one that means the same thing.
        let forms = crate::operator_named_forms(compound);
        let name = match forms.as_slice() {
            [only] => *only,
            _ => match crate::operator_builtin_named(compound, plain_impl) {
                Some(n) => n,
                None => continue,
            },
        };
        let want = crate::operator_builtin(name).map(|(_, canonical)| canonical);
        match binding(compound) {
            // Already imported, and it accumulates the same way.
            Some(c) if c == want => ops.push((op, compound, None)),
            // Bound to something else — another flavor, or a user function.
            Some(_) => {}
            None => ops.push((op, compound, Some(name))),
        }
    }
    CompoundOps { ops }
}
