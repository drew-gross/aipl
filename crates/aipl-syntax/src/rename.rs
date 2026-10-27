//! Renaming the locals of an expression.
//!
//! A name is a spelling, and moving code under other binders can change what a
//! spelling means. This is how a pass puts code somewhere new without that
//! happening: [`rename_params`] retargets references to chosen names, and
//! [`rename_scoped`] with `fresh` also gives every binder the code introduces a
//! name nothing else has, so the copy can neither shadow a binder around it nor
//! be shadowed by one.

use std::collections::HashMap;

use crate::ast::{Callee, Expr, ExprKind, FieldInit, LambdaParam, MatchArm};

/// A number no earlier call has returned. Process-wide rather than per pass or
/// per function: four passes inline, and a body one pass copied is copied again
/// by a later one, so counters that each start at zero would hand out the same
/// name twice in one function. Names only have to be unique within a program, so
/// sharing the counter between concurrent compiles costs nothing but a few
/// skipped numbers.
pub fn fresh_id() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// `e` with every binder it introduces renamed, for a copy of code that already
/// has another copy in the same function. Names it only mentions are left alone.
pub fn distinct_binders(e: &Expr) -> Expr {
    rename_scoped(e, &HashMap::new(), true)
}

/// A temporary nothing else is called: `$<stem><id>`.
pub fn fresh_temp(stem: &str) -> String {
    format!("${stem}{}", fresh_id())
}

/// A local name nothing else is called: `$<stem><id>_<base>`. `$` cannot appear
/// in a source identifier, so it cannot collide with one either.
pub fn fresh_local(stem: &str, base: &str) -> String {
    format!("${stem}{}_{base}", fresh_id())
}

/// Rebuild `e` (a function body) substituting parameter references per `map`
/// (original → fresh name), respecting shadowing: a `let`/`for`/`match`/lambda
/// binder of a mapped name removes it from the map within that scope (there the
/// name is the local, not the parameter). Body-internal binders are left as-is —
/// only the substituted *parameter* references are renamed.
pub fn rename_params(e: &Expr, map: &HashMap<String, String>) -> Expr {
    rename_scoped(e, map, false)
}

/// [`rename_params`], and with `fresh` also every binder the body introduces: each
/// `let`, loop variable, pattern binder and lambda parameter becomes a new
/// `$inl<N>_<name>`, so a body copied under a caller's variables can neither
/// shadow one nor be shadowed, and two copies of one body share no binder.
pub fn rename_scoped(e: &Expr, map: &HashMap<String, String>, fresh: bool) -> Expr {
    let sub = |n: &String| map.get(n).cloned().unwrap_or_else(|| n.clone());
    // The map inside a scope that binds `names`: they are the locals there, not
    // the parameters, and under `fresh` each is known by a new name, returned
    // alongside.
    let scope = |names: &[String]| -> (HashMap<String, String>, HashMap<String, String>) {
        let mut inner = map.clone();
        let mut renamed = HashMap::new();
        for n in names {
            inner.remove(n);
            if fresh && n != "_" {
                // A shim's closure is found by its prefix, which a fresh name keeps.
                let nm = match n.strip_prefix(crate::SHIM_CLOSURE_PREFIX) {
                    Some(rest) => {
                        format!("{}$inl{}_{rest}", crate::SHIM_CLOSURE_PREFIX, fresh_id())
                    }
                    None => format!("$inl{}_{}", fresh_id(), n),
                };
                inner.insert(n.clone(), nm.clone());
                renamed.insert(n.clone(), nm);
            }
        }
        (inner, renamed)
    };
    let known_as = |renamed: &HashMap<String, String>, n: &String| {
        renamed.get(n).cloned().unwrap_or_else(|| n.clone())
    };
    let kind = match &e.kind {
        ExprKind::KwArg(k, x, forwarded) => ExprKind::KwArg(
            k.clone(),
            Box::new(rename_scoped(x, map, fresh)),
            *forwarded,
        ),
        ExprKind::TypeArgs(x, tys) => {
            ExprKind::TypeArgs(Box::new(rename_scoped(x, map, fresh)), tys.clone())
        }
        ExprKind::Spread(x) => ExprKind::Spread(Box::new(rename_scoped(x, map, fresh))),
        // A shim binds a function by name: a global, which renaming never touches,
        // or a closure `let` the parser put just outside it, which it does.
        ExprKind::Shim(effect, bindings, body) => ExprKind::Shim(
            effect.clone(),
            bindings
                .iter()
                .map(|(op, f)| (op.clone(), sub(f)))
                .collect(),
            Box::new(rename_scoped(body, map, fresh)),
        ),
        ExprKind::Ident(n) => ExprKind::Ident(sub(n)),
        // Only a name can be a renamed parameter: a call through one is a
        // `User` callee, and every other arm names a function the compiler
        // knows, which no parameter can shadow.
        ExprKind::Call(name, args, m) => ExprKind::Call(
            match name {
                Callee::Local(n) => Callee::Local(sub(n)),
                known => known.clone(),
            },
            args.iter().map(|a| rename_scoped(a, map, fresh)).collect(),
            *m,
        ),
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => e.kind.clone(),
        ExprKind::Neg(x) => ExprKind::Neg(Box::new(rename_scoped(x, map, fresh))),
        ExprKind::Try(x) => ExprKind::Try(Box::new(rename_scoped(x, map, fresh))),
        ExprKind::Return(x) => ExprKind::Return(Box::new(rename_scoped(x, map, fresh))),
        ExprKind::Field(x, fld) => {
            ExprKind::Field(Box::new(rename_scoped(x, map, fresh)), fld.clone())
        }
        ExprKind::Seq(a, b) => ExprKind::Seq(
            Box::new(rename_scoped(a, map, fresh)),
            Box::new(rename_scoped(b, map, fresh)),
        ),
        ExprKind::Index(a, b) => ExprKind::Index(
            Box::new(rename_scoped(a, map, fresh)),
            Box::new(rename_scoped(b, map, fresh)),
        ),
        ExprKind::While(a, b) => ExprKind::While(
            Box::new(rename_scoped(a, map, fresh)),
            Box::new(rename_scoped(b, map, fresh)),
        ),
        ExprKind::If(a, b, c) => ExprKind::If(
            Box::new(rename_scoped(a, map, fresh)),
            Box::new(rename_scoped(b, map, fresh)),
            Box::new(rename_scoped(c, map, fresh)),
        ),
        ExprKind::Slice(a, b, c) => ExprKind::Slice(
            Box::new(rename_scoped(a, map, fresh)),
            Box::new(rename_scoped(b, map, fresh)),
            c.as_ref().map(|c| Box::new(rename_scoped(c, map, fresh))),
        ),
        ExprKind::ArrayLit(xs) => {
            ExprKind::ArrayLit(xs.iter().map(|x| rename_scoped(x, map, fresh)).collect())
        }
        ExprKind::SetLit(xs, o) => ExprKind::SetLit(
            xs.iter().map(|x| rename_scoped(x, map, fresh)).collect(),
            *o,
        ),
        ExprKind::TupleLit(xs) => {
            ExprKind::TupleLit(xs.iter().map(|x| rename_scoped(x, map, fresh)).collect())
        }
        ExprKind::DictLit(pairs) => ExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (rename_scoped(k, map, fresh), rename_scoped(v, map, fresh)))
                .collect(),
        ),
        ExprKind::Construct(name, inits) => ExprKind::Construct(
            name.clone(),
            inits
                .iter()
                .map(|i| FieldInit {
                    name: i.name.clone(),
                    value: rename_scoped(&i.value, map, fresh),
                })
                .collect(),
        ),
        // The LHS is a place expression — a bare ident, or a field chain rooted
        // at one. It used to be left alone on the grounds that it is rooted at a
        // `mut` local and parameters are immutable, but a mutating method's
        // `mut self` *is* a parameter: its body is a run of `set self... `, and
        // leaving those unrenamed would assign to a binding the inlined
        // expression never introduced. Renaming it is right either way — a
        // caller-side local is simply absent from the map.
        ExprKind::Assign(lhs, a, b) => ExprKind::Assign(
            Box::new(rename_scoped(lhs, map, fresh)),
            Box::new(rename_scoped(a, map, fresh)),
            Box::new(rename_scoped(b, map, fresh)),
        ),
        ExprKind::Let(n, ty, a, b) => {
            let (inner, renamed) = scope(std::slice::from_ref(n));
            ExprKind::Let(
                known_as(&renamed, n),
                ty.clone(),
                Box::new(rename_scoped(a, map, fresh)),
                Box::new(rename_scoped(b, &inner, fresh)),
            )
        }
        ExprKind::LetMut(n, ty, a, b) => {
            let (inner, renamed) = scope(std::slice::from_ref(n));
            ExprKind::LetMut(
                known_as(&renamed, n),
                ty.clone(),
                Box::new(rename_scoped(a, map, fresh)),
                Box::new(rename_scoped(b, &inner, fresh)),
            )
        }
        ExprKind::For(v, index, iter, b) => {
            // Both binders shadow a parameter of the same name.
            let (inner, renamed) = scope(&crate::loop_binders(v, index));
            ExprKind::For(
                known_as(&renamed, v),
                index.as_ref().map(|i| known_as(&renamed, i)),
                Box::new(rename_scoped(iter, map, fresh)),
                Box::new(rename_scoped(b, &inner, fresh)),
            )
        }
        ExprKind::Match(s, arms) => ExprKind::Match(
            Box::new(rename_scoped(s, map, fresh)),
            arms.iter().map(|a| arm_scoped(a, &scope, fresh)).collect(),
        ),
        ExprKind::IfLet(arm, s, else_b) => ExprKind::IfLet(
            Box::new(arm_scoped(arm, &scope, fresh)),
            Box::new(rename_scoped(s, map, fresh)),
            Box::new(rename_scoped(else_b, map, fresh)),
        ),
        ExprKind::Lambda(ps, b) => {
            let names: Vec<String> = ps.iter().map(|p| p.name.clone()).collect();
            let (inner, renamed) = scope(&names);
            let ps = ps
                .iter()
                .map(|p| LambdaParam {
                    name: known_as(&renamed, &p.name),
                    ..p.clone()
                })
                .collect();
            ExprKind::Lambda(ps, Box::new(rename_scoped(b, &inner, fresh)))
        }
    };
    Expr::rebuilt(kind, e)
}

/// A match arm under [`rename_scoped`]: its pattern's binders open a scope over
/// the body.
fn arm_scoped(
    arm: &MatchArm,
    scope: &dyn Fn(&[String]) -> (HashMap<String, String>, HashMap<String, String>),
    fresh: bool,
) -> MatchArm {
    let (inner, renamed) = scope(&arm.pattern.bindings());
    MatchArm {
        pattern: arm
            .pattern
            .rename_bindings(&mut |n| renamed.get(n).cloned().unwrap_or_else(|| n.to_string())),
        body: rename_scoped(&arm.body, &inner, fresh),
        span: arm.span.clone(),
    }
}
