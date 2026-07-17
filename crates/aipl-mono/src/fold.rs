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
//!
//! String concat, constant branch elimination, and propagation through
//! bindings are out of scope for now.

use aipl_syntax::ast::{Expr, ExprKind, FieldDecl, FieldInit, Item, MatchArm, Program};

/// Fold constant subexpressions throughout `program`: every function body and
/// test body, and every struct field default.
pub fn fold_constants(program: &Program) -> Program {
    Program {
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) => {
                    let mut f = f.clone();
                    f.body = fold_expr(&f.body);
                    f.test_body = f.test_body.as_ref().map(fold_expr);
                    Item::Fn(f)
                }
                Item::Struct(s) => {
                    let mut s = s.clone();
                    s.fields = s
                        .fields
                        .iter()
                        .map(|fd| FieldDecl {
                            name: fd.name.clone(),
                            ty: fd.ty.clone(),
                            default: fd.default.as_ref().map(fold_expr),
                        })
                        .collect();
                    Item::Struct(s)
                }
                other => other.clone(),
            })
            .collect(),
    }
}

/// Structurally rebuild `e` with children folded first, then fold this node
/// itself if it is a constant op over literals.
fn fold_expr(e: &Expr) -> Expr {
    let f = |x: &Expr| Box::new(fold_expr(x));
    let kind = match &e.kind {
        ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::None
        | ExprKind::Unit => e.kind.clone(),
        ExprKind::Call(name, args, ms) => {
            ExprKind::Call(name.clone(), args.iter().map(fold_expr).collect(), *ms)
        }
        ExprKind::Binop(a, op, b) => ExprKind::Binop(f(a), *op, f(b)),
        ExprKind::Neg(x) => ExprKind::Neg(f(x)),
        ExprKind::Not(x) => ExprKind::Not(f(x)),
        ExprKind::If(c, t, e2) => ExprKind::If(f(c), f(t), f(e2)),
        ExprKind::Construct(name, inits) => ExprKind::Construct(
            name.clone(),
            inits
                .iter()
                .map(|i| FieldInit {
                    name: i.name.clone(),
                    value: fold_expr(&i.value),
                })
                .collect(),
        ),
        ExprKind::Field(x, field) => ExprKind::Field(f(x), field.clone()),
        ExprKind::Let(n, v, b) => ExprKind::Let(n.clone(), f(v), f(b)),
        ExprKind::LetMut(n, v, b) => ExprKind::LetMut(n.clone(), f(v), f(b)),
        // The LHS is a place (idents/fields only) — nothing to fold there.
        ExprKind::Assign(lhs, v, b) => ExprKind::Assign(lhs.clone(), f(v), f(b)),
        ExprKind::For(v, iter, b) => ExprKind::For(v.clone(), f(iter), f(b)),
        ExprKind::While(c, b) => ExprKind::While(f(c), f(b)),
        // Arm patterns are literal-only (checker-enforced) — fold the bodies.
        ExprKind::Match(scrut, arms) => ExprKind::Match(
            f(scrut),
            arms.iter()
                .map(|arm| MatchArm {
                    pattern: arm.pattern.clone(),
                    body: fold_expr(&arm.body),
                    span: arm.span.clone(),
                })
                .collect(),
        ),
        ExprKind::ArrayLit(elems) => ExprKind::ArrayLit(elems.iter().map(fold_expr).collect()),
        ExprKind::SetLit(elems) => ExprKind::SetLit(elems.iter().map(fold_expr).collect()),
        ExprKind::TupleLit(elems) => ExprKind::TupleLit(elems.iter().map(fold_expr).collect()),
        ExprKind::DictLit(pairs) => ExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (fold_expr(k), fold_expr(v)))
                .collect(),
        ),
        ExprKind::Index(a, b) => ExprKind::Index(f(a), f(b)),
        ExprKind::Slice(a, b, c) => ExprKind::Slice(f(a), f(b), c.as_deref().map(f)),
        ExprKind::Try(x) => ExprKind::Try(f(x)),
        ExprKind::Seq(a, b) => ExprKind::Seq(f(a), f(b)),
        ExprKind::Return(x) => ExprKind::Return(f(x)),
        ExprKind::Lambda(params, body) => ExprKind::Lambda(params.clone(), f(body)),
    };
    let kind = try_fold(&kind).unwrap_or(kind);
    Expr::new(kind, e.span.clone())
}

/// The folded form of `kind` (whose children are already folded), if it is a
/// constant op over literals; `None` leaves it unchanged.
fn try_fold(kind: &ExprKind) -> Option<ExprKind> {
    match kind {
        // Codegen's unary `-` is `ineg` (two's-complement), so wrap.
        ExprKind::Neg(x) => match &x.kind {
            ExprKind::Num(n) => Some(ExprKind::Num(n.wrapping_neg())),
            _ => None,
        },
        ExprKind::Not(x) => match &x.kind {
            ExprKind::Bool(b) => Some(ExprKind::Bool(!b)),
            _ => None,
        },
        ExprKind::Binop(l, op, r) => fold_binop(l, *op, r),
        // The reserved impls the loader resolves `+`/`-` to. Only these names:
        // an operator aliased to a user function is an ordinary call.
        ExprKind::Call(name, args, _) => {
            let [a, b] = args.as_slice() else {
                return None;
            };
            let (ExprKind::Num(a), ExprKind::Num(b)) = (&a.kind, &b.kind) else {
                return None;
            };
            let n = match name.as_str() {
                "__builtin_wrapping_add" => a.wrapping_add(*b),
                "__builtin_saturating_add" => a.saturating_add(*b),
                "__builtin_wrapping_sub" => a.wrapping_sub(*b),
                "__builtin_saturating_sub" => a.saturating_sub(*b),
                _ => return None,
            };
            Some(ExprKind::Num(n))
        }
        _ => None,
    }
}

/// Fold a binary op over two literals. Integer ops use `i64` semantics — the
/// only type a literal-literal op can have (see the module docs).
fn fold_binop(l: &Expr, op: char, r: &Expr) -> Option<ExprKind> {
    match (&l.kind, &r.kind) {
        (ExprKind::Num(a), ExprKind::Num(b)) => {
            let (a, b) = (*a, *b);
            Some(match op {
                '+' => ExprKind::Num(a.wrapping_add(b)),
                '-' => ExprKind::Num(a.wrapping_sub(b)),
                '*' => ExprKind::Num(a.wrapping_mul(b)),
                // `sdiv`/`srem` trap on a zero divisor and on `i64::MIN / -1`;
                // `checked_*` returns `None` exactly then, leaving the trap to
                // run time.
                '/' => ExprKind::Num(a.checked_div(b)?),
                '%' => ExprKind::Num(a.checked_rem(b)?),
                '<' => ExprKind::Bool(a < b),
                '>' => ExprKind::Bool(a > b),
                'L' => ExprKind::Bool(a <= b),
                'G' => ExprKind::Bool(a >= b),
                'E' => ExprKind::Bool(a == b),
                'N' => ExprKind::Bool(a != b),
                _ => return None,
            })
        }
        // With both sides literal, `&&`/`||` short-circuiting is unobservable.
        (ExprKind::Bool(a), ExprKind::Bool(b)) => Some(match op {
            'A' => ExprKind::Bool(*a && *b),
            'O' => ExprKind::Bool(*a || *b),
            'E' => ExprKind::Bool(a == b),
            'N' => ExprKind::Bool(a != b),
            _ => return None,
        }),
        _ => None,
    }
}
