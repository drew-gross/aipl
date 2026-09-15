//! `count` against a comparison: `xs.count(x) < 4` is settled the moment a
//! fourth match appears, so the pair collapses into `count_is_less_than`.

use aipl_syntax::ast::{BinOp, Callee, Expr, ExprKind};

/// One fusable shape: a call to `call`, compared with `op` against a second
/// operand, becomes a call to `into` taking that operand as a trailing argument.
///
/// `op` is written with the call on the **left**; [`build`] mirrors it.
struct Fusion {
    call: Callee,
    op: BinOp,
    into: Callee,
}

/// Every shape the pass knows. See the module docs for how to add one.
const COMPARISON_FUSIONS: &[Fusion] = &[
    Fusion {
        call: Callee::Count,
        op: BinOp::Lt,
        into: Callee::CountIsLessThan,
    },
    Fusion {
        call: Callee::Count,
        op: BinOp::Le,
        into: Callee::CountIsAtMost,
    },
    Fusion {
        call: Callee::Count,
        op: BinOp::Gt,
        into: Callee::CountIsGreaterThan,
    },
    Fusion {
        call: Callee::Count,
        op: BinOp::Ge,
        into: Callee::CountIsAtLeast,
    },
    Fusion {
        call: Callee::Count,
        op: BinOp::Eq,
        into: Callee::CountIsEqual,
    },
    Fusion {
        call: Callee::Count,
        op: BinOp::Ne,
        into: Callee::CountIsNotEqual,
    },
];

/// `l OP r` as a fused call, whichever operand is the call: the left one reads
/// with `op` as written, the right one is the same claim with the operator
/// mirrored.
pub(super) fn build(l: &Expr, op: BinOp, r: &Expr, whole: &Expr) -> Option<Expr> {
    build_one(l, op, r, whole).or_else(|| build_one(r, flip(op), l, whole))
}

/// `a OP b` and `b flip(OP) a` are the same claim, which is what lets one
/// [`COMPARISON_FUSIONS`] row cover both operand orders. `==`/`!=` are symmetric.
fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Gt => BinOp::Lt,
        BinOp::Le => BinOp::Ge,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}

/// `call OP bound` as a fused call, when `call` is a [`COMPARISON_FUSIONS`] row
/// for `OP`.
fn build_one(call: &Expr, op: BinOp, bound: &Expr, whole: &Expr) -> Option<Expr> {
    let ExprKind::Call(name, args, method_style) = &call.kind else {
        return None;
    };
    let f = COMPARISON_FUSIONS
        .iter()
        .find(|f| f.call == *name && f.op == op)?;
    let mut fused = args.clone();
    fused.push(bound.clone());
    // Spanned as the whole comparison: that is the source the user wrote, and
    // what any later diagnostic should point at.
    Some(Expr::rebuilt(
        ExprKind::Call(f.into.clone(), fused, *method_style),
        whole,
    ))
}
