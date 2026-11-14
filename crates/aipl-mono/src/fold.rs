//! Constant folding: evaluate constant subexpressions at compile time.
//!
//! Runs on the loaded, type-checked source program (before monomorphization),
//! so the typing rules are already settled: a literal-literal integer op is
//! always `i64` — a bare literal only narrows against a *non-literal* operand
//! (see `flex_int_ty`), and two literals stay `i64` — which makes `i64`
//! semantics the correct fold semantics. Folding is bottom-up, so chains like
//! `1 + 2 + 3` collapse fully.
//!
//! What folds (deliberately basic for now):
//! - integer arithmetic on two `Num` literals: `+`/`-`/`*` wrap (codegen's
//!   plain `iadd`/`isub`/`imul` on i64 registers), `/`/`%` fold only when the
//!   run-time op couldn't trap (nonzero divisor, not `i64::MIN / -1`)
//! - integer comparisons on two `Num` literals, `&&`/`||`/`==`/`!=` on two
//!   `Bool` literals (with both sides literal, short-circuiting is
//!   unobservable)
//! - `-` / `!` on a literal
//! - the reserved integer-arithmetic builtins the loader resolves `+`/`-` to
//!   (`__builtin_wrapping_add` etc.) with two `Num` arguments — only the
//!   `__builtin_*` names, since an operator aliased to a *user* function is an
//!   ordinary call
//! - a `match` whose scrutinee is a `str`, `char` or integer literal: the first
//!   arm whose pattern accepts it, and nothing else ([`fold_match`]). The
//!   scrutinee may be a `let`-bound `str` literal used for nothing but the
//!   `match` — the shape an inlined call leaves
//!
//! The `rope_str` builtin is how a program asks for a run-time concat by name;
//! `heap_str` and `inline_str` are its siblings. See
//! `tests/cases/strings/named_representations.aipl`, and DESIGN_PRINCIPLES.md's
//! "Just say what you want" for why naming the result beats arranging for an
//! optimization to miss.
//!
//! **A template literal does not fold**, even when it is constant throughout.
//! It lowers to a concat chain over *rendered* pieces (`__template_interp`), and
//! folding one of those would mean a second implementation of the renderer —
//! `aipl_write_i64`'s formatting, `"true"`/`"false"`, a `char`'s single raw
//! byte — living here, agreeing byte-for-byte with the run-time one forever,
//! with the fold itself removing the cases that would have caught a divergence
//! (`strings/int_to_str/negatives` renders `i64::MIN` through `fmt_i64` only so
//! long as that call survives to run time). The joiner the chain is built from
//! (`__aipl_concat`) is deliberately not folded here either: without a rendered
//! piece folding first, it never has two literals to join.

use crate::passes::Scope;
use crate::MonoProgram;
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TPattern, TyRepr};

use std::collections::HashMap;

use aipl_syntax::ast::{BinOp, Callee, Primitive};

/// [`fold_match`] over the monomorphized program — and only that.
///
/// Inlining after monomorphization (`inline_small_post_mono`) is what puts a
/// literal argument next to the body it feeds for any function that only became
/// small once the earlier folds had run — a `match` that answers `true` or
/// `false` per arm, say — and the pre-mono folder never sees those bodies. This
/// is the half of folding that has to run on them.
///
/// Only matches, not [`fold_constants`] as a whole. The rest would be sound here
/// too, but it would also propagate a literal through the inlined `measure` of a
/// `.benchmark` block into the closure it measures, and `measure("..", 1, |n|
/// [n, n + 1])` is written with a literal precisely so that the array is built at
/// run time. Folding it to a constant array would make every such claim measure
/// nothing. Choosing an arm moves no literal into anything it was not already
/// in.
pub fn fold_matches_post_mono(program: &MonoProgram, scope: &Scope) -> MonoProgram {
    crate::typed_util::map_bodies(
        program,
        |name| scope.covers(name),
        |_, body| fold_matches_typed(body),
    )
}

/// [`fold_matches_post_mono`] on the pinned program, before monomorphization.
pub fn fold_matches_pinned(
    program: &aipl_syntax::typed::TProgram,
    scope: &Scope,
) -> aipl_syntax::typed::TProgram {
    crate::typed_util::map_pinned_bodies(
        program,
        |name| scope.covers(name),
        |_, body| fold_matches_typed(body),
    )
}

/// `e` with every `match` on a literal replaced by the arm that takes it,
/// innermost first.
fn fold_matches_typed<T: TyRepr>(e: &mut TExpr<T>) {
    crate::typed_util::each_child_mut(e, &mut fold_matches_typed);
    // A `let` of a `str` literal used only as the scrutinee of its body's `match`
    // is that `match` on the literal (see `is_match_only_use`).
    let folded = match &e.kind {
        TExprKind::Let(name, value, body)
            if is_match_only_use_typed(value, e.annotation.as_ref(), body, name) =>
        {
            match &body.kind {
                TExprKind::Match(_, arms) => Some(
                    fold_match_typed(value, arms)
                        .unwrap_or_else(|| TExprKind::Match(value.clone(), arms.clone())),
                ),
                _ => None,
            }
        }
        TExprKind::Match(scrutinee, arms) => fold_match_typed(scrutinee, arms),
        _ => None,
    };
    if let Some(kind) = folded {
        *e = TExpr::rebuilt(kind, e.ty.clone(), e);
    }
}

// The pinned-tree twin of the folder, run before monomorphization. A literal-literal integer
// operation is folded as `i64` arithmetic, whatever width the context fitted its operands to,
// since the backend has no arithmetic for an operation on two literals of another width.

use aipl_syntax::typed::{TFieldInit, Ty};

type Pinned = TExpr<Ty>;

/// [`fold_constants`] on the pinned program.
pub fn fold_constants_pinned(
    program: &aipl_syntax::typed::TProgram,
    scope: &Scope,
) -> aipl_syntax::typed::TProgram {
    use aipl_syntax::typed::TItem;
    let mut out = program.clone();
    for item in &mut out.items {
        match item {
            TItem::Fn(f) if scope.covers(&f.name.text) => {
                f.body = fold_pinned(&f.body, &HashMap::new());
                f.test_body = f
                    .test_body
                    .as_ref()
                    .map(|x| fold_pinned(x, &HashMap::new()));
            }
            TItem::Struct(s) => {
                for field in &mut s.fields {
                    field.default = field
                        .default
                        .as_ref()
                        .map(|x| fold_pinned(x, &HashMap::new()));
                }
            }
            _ => {}
        }
    }
    out
}

fn i64_ty() -> Ty {
    Ty::Primitive(Primitive::I64)
}

/// [`fold_expr`] over the pinned tree.
fn fold_pinned(e: &Pinned, env: &HashMap<String, Pinned>) -> Pinned {
    let f = |x: &Pinned| Box::new(fold_pinned(x, env));
    let arm = |a: &TMatchArm<Ty>| TMatchArm {
        pattern: a.pattern.clone(),
        body: fold_pinned(&a.body, env),
        span: a.span.clone(),
    };
    let kind = match &e.kind {
        TExprKind::Shim(effect, bindings, body) => {
            TExprKind::Shim(effect.clone(), bindings.clone(), f(body))
        }
        // A propagated binding's literal takes the identifier's place.
        TExprKind::Ident(n) => match env.get(n) {
            Some(literal) => return TExpr::rebuilt(literal.kind.clone(), e.ty.clone(), e),
            None => e.kind.clone(),
        },
        TExprKind::Int(_)
        | TExprKind::Float(_)
        | TExprKind::Bool(_)
        | TExprKind::Str(_)
        | TExprKind::Char(_)
        | TExprKind::None
        | TExprKind::Unit => e.kind.clone(),
        TExprKind::Call {
            callee,
            type_args,
            args,
            method_style,
        } => TExprKind::Call {
            callee: callee.clone(),
            type_args: type_args.clone(),
            args: args.iter().map(|x| fold_pinned(x, env)).collect(),
            method_style: *method_style,
        },
        TExprKind::Neg(x) => TExprKind::Neg(f(x)),
        TExprKind::If(c, t, e2) => TExprKind::If(f(c), f(t), f(e2)),
        TExprKind::Construct(name, inits) => TExprKind::Construct(
            name.clone(),
            inits
                .iter()
                .map(|i| TFieldInit {
                    name: i.name.clone(),
                    value: fold_pinned(&i.value, env),
                })
                .collect(),
        ),
        TExprKind::Field(x, field) => TExprKind::Field(f(x), field.clone()),
        TExprKind::Let(n, v, b) => {
            let v = fold_pinned(v, env);
            // `let x = <literal>; body` is the body with `x` substituted (see `fold_expr`).
            if (propagatable_pinned(&v, e.annotation.as_ref())
                || is_match_only_use_typed(&v, e.annotation.as_ref(), b, n))
                && !binds_name_pinned(b, n)
            {
                let mut inner = env.clone();
                inner.insert(n.clone(), v);
                return fold_pinned(b, &inner);
            }
            TExprKind::Let(n.clone(), Box::new(v), f(b))
        }
        TExprKind::LetMut(n, v, b) => TExprKind::LetMut(n.clone(), f(v), f(b)),
        TExprKind::Assign(lhs, v, b) => TExprKind::Assign(lhs.clone(), f(v), f(b)),
        TExprKind::For(v, i, iter, b) => TExprKind::For(v.clone(), i.clone(), f(iter), f(b)),
        TExprKind::While(c, b) => TExprKind::While(f(c), f(b)),
        TExprKind::Match(scrut, arms) => TExprKind::Match(f(scrut), arms.iter().map(arm).collect()),
        TExprKind::IfLet(a, scrut, else_b) => {
            TExprKind::IfLet(Box::new(arm(a)), f(scrut), f(else_b))
        }
        TExprKind::ArrayLit(elems) => {
            TExprKind::ArrayLit(elems.iter().map(|x| fold_pinned(x, env)).collect())
        }
        TExprKind::SetLit(elems, o) => {
            TExprKind::SetLit(elems.iter().map(|x| fold_pinned(x, env)).collect(), *o)
        }
        TExprKind::TupleLit(elems) => {
            TExprKind::TupleLit(elems.iter().map(|x| fold_pinned(x, env)).collect())
        }
        TExprKind::DictLit(pairs) => TExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (fold_pinned(k, env), fold_pinned(v, env)))
                .collect(),
        ),
        TExprKind::Index(a, b) => TExprKind::Index(f(a), f(b)),
        TExprKind::Slice(a, b, c) => TExprKind::Slice(f(a), f(b), c.as_deref().map(&f)),
        TExprKind::Try(x) => TExprKind::Try(f(x)),
        TExprKind::Seq(a, b) => TExprKind::Seq(f(a), f(b)),
        TExprKind::Return(x) => TExprKind::Return(f(x)),
        TExprKind::Lambda(params, body) => TExprKind::Lambda(params.clone(), f(body)),
    };
    let mut out = TExpr::rebuilt(kind, e.ty.clone(), e);
    out.annotation = e.annotation.clone();
    out.unsettled = e.unsettled;
    if let Some(folded) = try_fold_pinned(&out) {
        return TExpr::rebuilt(folded.0, folded.1, e);
    }
    out
}

/// [`propagatable`] over the pinned tree: a literal substitutes for a binding when it is the
/// natural type of the literal and the binding says nothing else.
fn propagatable_pinned(v: &Pinned, annotation: Option<&Ty>) -> bool {
    let natural = match &v.kind {
        TExprKind::Int(_) => i64_ty(),
        TExprKind::Bool(_) => Ty::Primitive(Primitive::Bool),
        TExprKind::Char(_) => Ty::Primitive(Primitive::Char),
        _ => return false,
    };
    v.ty == natural && annotation.is_none_or(|t| *t == natural)
}

/// [`binds_name`] over the pinned tree.
fn binds_name_pinned(e: &Pinned, name: &str) -> bool {
    let pattern = |p: &TPattern<Ty>| match p {
        TPattern::Array(elems) | TPattern::ArrayRest(elems) => elems
            .iter()
            .any(|el| matches!(&el.kind, TExprKind::Ident(n) if n == name)),
        other => other.bindings().iter().any(|b| b == name),
    };
    let here = match &e.kind {
        TExprKind::Let(n, _, _) | TExprKind::LetMut(n, _, _) => n == name,
        TExprKind::For(n, index, _, _) => aipl_syntax::for_binds(n, index, name),
        TExprKind::Lambda(params, _) => params.iter().any(|p| p.name == name),
        TExprKind::Match(_, arms) => arms.iter().any(|arm| pattern(&arm.pattern)),
        TExprKind::IfLet(arm, _, _) => pattern(&arm.pattern),
        _ => false,
    };
    here || crate::typed_util::any_child(e, |c| binds_name_pinned(c, name))
}

/// [`try_fold`] over the pinned tree: the folded kind and its type.
fn try_fold_pinned(e: &Pinned) -> Option<(TExprKind<Ty>, Ty)> {
    match &e.kind {
        TExprKind::Neg(x) => match &x.kind {
            TExprKind::Int(n) => Some((TExprKind::Int(n.wrapping_neg()), e.ty.clone())),
            _ => None,
        },
        TExprKind::If(c, t, f) => {
            let taken = match c.kind {
                TExprKind::Bool(true) => t,
                TExprKind::Bool(false) => f,
                _ => return None,
            };
            let unifies_types = crate::typed_util::contains_context_literal(t)
                || crate::typed_util::contains_context_literal(f);
            (!unifies_types).then(|| (taken.kind.clone(), taken.ty.clone()))
        }
        TExprKind::Match(scrutinee, arms) => {
            fold_match_typed(scrutinee, arms).map(|kind| (kind, e.ty.clone()))
        }
        TExprKind::Call { callee, args, .. } => {
            if *callee == Callee::LogicalNot {
                let [x] = args.as_slice() else { return None };
                let TExprKind::Bool(b) = &x.kind else {
                    return None;
                };
                return Some((TExprKind::Bool(!b), e.ty.clone()));
            }
            let [a, b] = args.as_slice() else { return None };
            if let Some(op) = aipl_syntax::binop_for_builtin(callee) {
                return fold_binop_pinned(a, op, b, &e.ty);
            }
            let (TExprKind::Int(a), TExprKind::Int(b)) = (&a.kind, &b.kind) else {
                return None;
            };
            let n = match callee {
                Callee::WrappingAdd => a.wrapping_add(*b),
                Callee::SaturatingAdd => a.saturating_add(*b),
                Callee::WrappingSub => a.wrapping_sub(*b),
                Callee::SaturatingSub => a.saturating_sub(*b),
                Callee::WrappingMul => a.wrapping_mul(*b),
                _ => return None,
            };
            Some((TExprKind::Int(n), e.ty.clone()))
        }
        _ => None,
    }
}

/// [`fold_binop`] over the pinned tree; `ty` is the type of the operation.
fn fold_binop_pinned(l: &Pinned, op: BinOp, r: &Pinned, ty: &Ty) -> Option<(TExprKind<Ty>, Ty)> {
    let kind = match (&l.kind, &r.kind) {
        (TExprKind::Int(a), TExprKind::Int(b)) => {
            let (a, b) = (*a, *b);
            match op {
                BinOp::Add => TExprKind::Int(a.wrapping_add(b)),
                BinOp::Sub => TExprKind::Int(a.wrapping_sub(b)),
                BinOp::Mul => TExprKind::Int(a.wrapping_mul(b)),
                BinOp::Div => TExprKind::Int(a.checked_div(b).unwrap_or(i64::MAX)),
                BinOp::Rem => {
                    TExprKind::Int(a.checked_rem(b).unwrap_or(if b == 0 { a } else { 0 }))
                }
                BinOp::Lt => TExprKind::Bool(a < b),
                BinOp::Gt => TExprKind::Bool(a > b),
                BinOp::Le => TExprKind::Bool(a <= b),
                BinOp::Ge => TExprKind::Bool(a >= b),
                BinOp::Eq => TExprKind::Bool(a == b),
                BinOp::Ne => TExprKind::Bool(a != b),
                _ => return None,
            }
        }
        (TExprKind::Bool(a), TExprKind::Bool(b)) => match op {
            BinOp::And => TExprKind::Bool(*a && *b),
            BinOp::Or => TExprKind::Bool(*a || *b),
            BinOp::Eq => TExprKind::Bool(a == b),
            BinOp::Ne => TExprKind::Bool(a != b),
            _ => return None,
        },
        _ => return None,
    };
    Some((kind, ty.clone()))
}

/// [`is_match_only_use`] over the typed tree, with the binding's `annotation`.
fn is_match_only_use_typed<T: TyRepr>(
    v: &TExpr<T>,
    annotation: Option<&T>,
    body: &TExpr<T>,
    name: &str,
) -> bool {
    if !matches!(v.kind, TExprKind::Str(_)) || annotation.is_some_and(|a| !a.is_str()) {
        return false;
    }
    let TExprKind::Match(scrutinee, arms) = &body.kind else {
        return false;
    };
    matches!(&scrutinee.kind, TExprKind::Ident(n) if n == name)
        && !arms
            .iter()
            .any(|a| crate::typed_util::mentions_free(&a.body, name))
}

/// [`fold_match`] over the typed tree.
fn fold_match_typed<T: TyRepr>(
    scrutinee: &TExpr<T>,
    arms: &[TMatchArm<T>],
) -> Option<TExprKind<T>> {
    let accepts = |pattern: &TPattern<T>| -> Option<bool> {
        match (&scrutinee.kind, pattern) {
            (_, TPattern::Wildcard) => Some(true),
            (TExprKind::Str(s), TPattern::Str(p)) => Some(s == p),
            (TExprKind::Char(c), TPattern::Char(p)) => Some(c == p),
            (TExprKind::Int(n), TPattern::Int(p)) => Some(n == p),
            _ => None,
        }
    };
    let mut taken = None;
    for arm in arms {
        if accepts(&arm.pattern)? {
            taken = Some(arm);
            break;
        }
    }
    let taken = taken?;
    if arms
        .iter()
        .any(|a| crate::typed_util::contains_context_literal(&a.body))
    {
        return None;
    }
    Some(taken.body.kind.clone())
}
