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

use std::collections::HashMap;

use aipl_syntax::ast::{
    Expr, ExprKind, FieldDecl, FieldInit, Item, MatchArm, Pattern, Primitive, Program, Type,
};

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
                    f.body = fold_expr(&f.body, &HashMap::new());
                    f.test_body = f.test_body.as_ref().map(|x| fold_expr(x, &HashMap::new()));
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
                            default: fd.default.as_ref().map(|x| fold_expr(x, &HashMap::new())),
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
fn fold_expr(e: &Expr, env: &HashMap<String, ExprKind>) -> Expr {
    let f = |x: &Expr| Box::new(fold_expr(x, env));
    let kind = match &e.kind {
        ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
        ExprKind::Spread(..) => unreachable!("array spreads are desugared by the loader"),
        // Nothing to fold in a shim's bindings — they are function names.
        ExprKind::Shim(effect, bindings, body) => {
            ExprKind::Shim(effect.clone(), bindings.clone(), f(body))
        }
        // A propagated binding's literal takes the identifier's place; every
        // other name stands for itself.
        ExprKind::Ident(n) => env.get(n).cloned().unwrap_or_else(|| e.kind.clone()),
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => e.kind.clone(),
        ExprKind::Call(name, args, ms) => ExprKind::Call(
            name.clone(),
            args.iter().map(|x| fold_expr(x, env)).collect(),
            *ms,
        ),
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
                    value: fold_expr(&i.value, env),
                })
                .collect(),
        ),
        ExprKind::Field(x, field) => ExprKind::Field(f(x), field.clone()),
        ExprKind::Let(n, ty, v, b) => {
            let v = fold_expr(v, env);
            // `let x = <literal>; body` → the body with `x` substituted, and the
            // binding gone. Chiefly for inlined calls: `double(21)` becomes
            // `let $inl0_x: i64 = 21; $inl0_x * 2`, which folds to `42` only
            // once the binding is out of the way. Folding the body under the
            // extended environment does both in one pass — the substituted
            // literals *are* what makes the enclosing ops foldable.
            if propagatable(&v, ty.as_ref()) && !binds_name(b, n) {
                let mut inner = env.clone();
                inner.insert(n.clone(), v.kind.clone());
                return fold_expr(b, &inner);
            }
            ExprKind::Let(n.clone(), ty.clone(), Box::new(v), f(b))
        }
        ExprKind::LetMut(n, ty, v, b) => ExprKind::LetMut(n.clone(), ty.clone(), f(v), f(b)),
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
                    body: fold_expr(&arm.body, env),
                    span: arm.span.clone(),
                })
                .collect(),
        ),
        ExprKind::ArrayLit(elems) => {
            ExprKind::ArrayLit(elems.iter().map(|x| fold_expr(x, env)).collect())
        }
        ExprKind::SetLit(elems) => {
            ExprKind::SetLit(elems.iter().map(|x| fold_expr(x, env)).collect())
        }
        ExprKind::TupleLit(elems) => {
            ExprKind::TupleLit(elems.iter().map(|x| fold_expr(x, env)).collect())
        }
        ExprKind::DictLit(pairs) => ExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (fold_expr(k, env), fold_expr(v, env)))
                .collect(),
        ),
        ExprKind::Index(a, b) => ExprKind::Index(f(a), f(b)),
        ExprKind::Slice(a, b, c) => ExprKind::Slice(f(a), f(b), c.as_deref().map(&f)),
        ExprKind::Try(x) => ExprKind::Try(f(x)),
        ExprKind::Seq(a, b) => ExprKind::Seq(f(a), f(b)),
        ExprKind::Return(x) => ExprKind::Return(f(x)),
        ExprKind::Lambda(params, body) => ExprKind::Lambda(params.clone(), f(body)),
    };
    let kind = try_fold(&kind).unwrap_or(kind);
    Expr::rebuilt(kind, e)
}

/// Whether `v` is a literal that may be substituted for a `let` binding of
/// declared type `ty`.
///
/// The annotation is the catch. [`build_inlined`] binds an inlined parameter
/// with its *declared* type precisely because a narrow one changes the
/// arithmetic: `let a: i8 = 100; a + a` adds at 8 bits, while the substituted
/// `100 + 100` would add at 64. So a literal only propagates through a binding
/// whose type it already carries — no annotation, or the literal's own natural
/// type.
///
/// [`build_inlined`]: crate::build_inlined
fn propagatable(v: &Expr, ty: Option<&Type>) -> bool {
    let natural = match &v.kind {
        ExprKind::Num(_) => Type::Primitive(Primitive::I64),
        ExprKind::Bool(_) => Type::Primitive(Primitive::Bool),
        ExprKind::Char(_) => Type::Primitive(Primitive::Char),
        // A `str` literal is a heap value: substituting it at N use sites turns
        // one materialization into N. Left alone — this pass is for scalars.
        _ => return false,
    };
    ty.is_none_or(|t| *t == natural)
}

/// Whether `e` rebinds `name` anywhere within it — a `let`/`mut`, a `for`
/// variable, a lambda parameter, or a `match` arm's payload binding.
///
/// Used as a blunt veto rather than tracking scopes through the substitution:
/// if the name is rebound anywhere in the body, no propagation happens at all.
/// Over-approximating costs a missed fold; getting scope tracking subtly wrong
/// costs a wrong program, and the bindings this pass exists for — the inliner's
/// `$inl<N>_<param>` — are unique by construction and never rebound.
fn binds_name(e: &Expr, name: &str) -> bool {
    let here = match &e.kind {
        ExprKind::Let(n, _, _, _) | ExprKind::LetMut(n, _, _, _) | ExprKind::For(n, _, _) => {
            n == name
        }
        ExprKind::Lambda(params, _) => params.iter().any(|p| p.name == name),
        ExprKind::Match(_, arms) => arms.iter().any(|arm| match &arm.pattern {
            Pattern::Ctor { bindings, .. } => bindings.iter().any(|b| b == name),
            // An array pattern's identifier elements are binders; its literal
            // elements are not, but treating both as binders only over-vetoes.
            Pattern::Array(elems) => elems
                .iter()
                .any(|el| matches!(&el.kind, ExprKind::Ident(n) if n == name)),
            Pattern::Str(_) | Pattern::Char(_) | Pattern::Wildcard => false,
        }),
        _ => false,
    };
    here || crate::children(e).iter().any(|c| binds_name(c, name))
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
        // `if (true) { a } else { b }` → `a`. The discarded branch is the one
        // that would not have run, so dropping it changes nothing observable —
        // and it removes a control-flow split, which is what later passes care
        // about: `ElideRcPairs` only cancels a retain against a release in the
        // *same* basic block, so collapsing an `if` can put the two together.
        //
        // Both branches must be free of context-typed literals. An `if` unifies
        // its branches' types, so `if (true) { none } else { some(1) }` gets the
        // `none`'s inner type from the arm being discarded; keeping only that
        // arm strands a `__none__` that codegen cannot render. Same reason
        // `inline_small` refuses such bodies.
        ExprKind::If(c, t, f) => {
            let taken = match c.kind {
                ExprKind::Bool(true) => t,
                ExprKind::Bool(false) => f,
                _ => return None,
            };
            let unifies_types =
                crate::contains_context_literal(t) || crate::contains_context_literal(f);
            (!unifies_types).then(|| taken.kind.clone())
        }
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
                "__builtin_wrapping_mul" => a.wrapping_mul(*b),
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
                // `/` saturates rather than trapping: a zero divisor and
                // `i64::MIN / -1` both answer `i64::MAX` (see
                // `saturating_div` in codegen, which this must agree with).
                // `checked_div` returns `None` in exactly those two cases.
                '/' => ExprKind::Num(a.checked_div(b).unwrap_or(i64::MAX)),
                // `srem` still traps on the same pairs, so leave those to run
                // time rather than fold an answer the runtime won't produce.
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
