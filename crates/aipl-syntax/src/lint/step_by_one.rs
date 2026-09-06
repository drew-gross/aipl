use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// A step of one written the long way — `set x = x + 1;`, `set x += 1;`, and
/// their `-` mirrors — where `set x++;` / `set x--;` is the form for it.
///
/// Every spelling that adds or subtracts exactly one is the same statement, so
/// they are one lint rather than four: the plain operator with either operand
/// order for `+` (`x + 1` and `1 + x` both add one) but only the left for `-`
/// (`set x = 1 - x;` is not a decrement), and the compound forms, whose
/// receiver is always on the left.
///
/// Only a bare-identifier LHS is flagged, because that is all `set x++;`
/// accepts — a field store (`set p.n = p.n + 1;`) has no shorter spelling to
/// recommend.
///
/// `steps` says which written operators have a step form that provably means
/// the same thing here, and names the import to add when the file lacks it —
/// see [`matching_steps`].
pub(super) fn step_by_one(e: &Expr, steps: &Steps, hits: &mut Vec<Error>) {
    let ExprKind::Assign(lhs, value, _) = &e.kind else {
        return;
    };
    let ExprKind::Ident(name) = &lhs.kind else {
        return;
    };
    // An operator use is a call whose callee is the spelling that was written —
    // which is exactly what this lint keys on, since `+` and `+=` want different
    // advice and are told apart by nothing else.
    let ExprKind::Call(written, args, _) = &value.kind else {
        return;
    };
    let [l, r] = args.as_slice() else {
        return;
    };
    let Some(step) = steps.get(written) else {
        return;
    };
    let is_one = |e: &Expr| matches!(e.kind, ExprKind::Num(1));
    let is_target = |e: &Expr| matches!(&e.kind, ExprKind::Ident(n) if n == name);
    let stepped = if is_target(l) {
        is_one(r)
    } else {
        // The other order is a step only where the operation is commutative:
        // `1 + x` adds one to `x`, `1 - x` does not subtract one from it.
        step.commutative && is_one(l) && is_target(r)
    };
    if !stepped {
        return;
    }
    // Name the import too when it's missing: neither step operator has a bare
    // form, so following the advice without it only trades this error for the
    // operator gate's.
    let import = step
        .import
        .map(|n| format!(", importing `{n} as {}` from builtins", step.spelling))
        .unwrap_or_default();
    let (what, form) = step.wording(name);
    hits.push(Error::at(
        format!(
            "{what} — use the {form} form \"set {name}{};\"{import} \
             (or append #[allow] to this line to keep it)",
            step.spelling
        ),
        e.span.clone(),
    ));
}

/// The advice for one written operator: the step spelling that replaces it,
/// whether the operation reads in either operand order, and the builtin to
/// import when the file doesn't have that spelling yet (`None` when it does).
#[derive(Clone, Copy)]
pub(super) struct Step {
    spelling: &'static str,
    commutative: bool,
    import: Option<&'static str>,
}

impl Step {
    /// What the statement does, and the name of the form that spells it — the
    /// two halves of the message that differ between the two directions.
    fn wording(&self, name: &str) -> (String, &'static str) {
        if self.spelling == "++" {
            (format!("adding 1 to {name:?}"), "increment")
        } else {
            (format!("subtracting 1 from {name:?}"), "decrement")
        }
    }
}

/// Which step-by-one rewrites this file can be advised to make, by the operator
/// each replaces. An operator absent from here is one this lint must stay quiet
/// about; built by [`matching_steps`].
#[derive(Default)]
pub(super) struct Steps {
    steps: Vec<(&'static str, Step)>,
}

impl Steps {
    /// The advice for the operator spelled `written`, if this lint has any here.
    pub(super) fn get(&self, written: &str) -> Option<Step> {
        self.steps
            .iter()
            .find(|(w, _)| *w == written)
            .map(|(_, s)| *s)
    }

    /// Nothing to advise anywhere — the driver skips the walk entirely.
    pub(super) fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// The step operator that steps the way each of this file's `+`, `+=`, `-` and
/// `-=` operates, and whether it still needs importing.
///
/// Both operators are bound by import and neither binding is fixed, so the two
/// spellings only agree when they resolve to the same implementation:
/// `wrapping_add as +` pairs with `wrapping_increment as ++`, `saturating_sub
/// as -` and `saturating_sub_assign as -=` both with `saturating_decrement as
/// --` — each pair sharing one `__builtin_*` (see `OPERATOR_BUILTINS`). The
/// lint stays quiet for an operator bound to a user function (no step flavor
/// could match it), and for one whose step spelling is already bound to the
/// *other* flavor, where the recommended form would step differently than the
/// `± 1` it replaces.
pub(super) fn matching_steps(program: &Program) -> Steps {
    // What each operator spelling's import resolves to: `None` for unbound, and
    // `Some(None)` for a binding that isn't an operator builtin at all (a user
    // function), which no step flavor can match.
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

    let mut steps = Vec::new();
    for (written, spelling, commutative) in [
        ("+", "++", true),
        ("+=", "++", false),
        ("-", "--", false),
        ("-=", "--", false),
    ] {
        // The written operator has to be this file's, and a builtin: a user's
        // `+` has no step flavor to pair with.
        let Some(Some(written_impl)) = binding(written) else {
            continue;
        };
        let Some(name) = crate::operator_builtin_named(spelling, written_impl) else {
            continue;
        };
        let want = crate::operator_builtin(name).map(|(_, canonical)| canonical);
        let import = match binding(spelling) {
            // Already imported, and it steps the same way: recommend the form
            // alone, so the message stays short.
            Some(c) if c == want => None,
            // Bound to something else — the other flavor, or a user function.
            Some(_) => continue,
            None => Some(name),
        };
        steps.push((
            written,
            Step {
                spelling,
                commutative,
                import,
            },
        ));
    }
    Steps { steps }
}
