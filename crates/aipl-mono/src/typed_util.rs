//! Walks over `TExpr<T>` the passes share, with the answers the `Expr` ones give.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{Callee, ConcreteType};
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TyRepr};

/// The tree everything after monomorphization reads.
pub type Typed = TExpr<ConcreteType>;

/// `program` with `rewrite` applied to the body of each function `covers` names; the others come
/// back as they were.
pub(crate) fn map_bodies(
    program: &crate::MonoProgram,
    covers: impl Fn(&str) -> bool,
    rewrite: impl Fn(&str, &mut Typed),
) -> crate::MonoProgram {
    crate::MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| {
                if !covers(&f.name) {
                    return f.clone();
                }
                let mut body = f.body.clone();
                rewrite(&f.name, &mut body);
                crate::ConcreteFn { body, ..f.clone() }
            })
            .collect(),
        ..program.clone()
    }
}

/// The sub-expressions of `e`, as [`crate::children`] has them: an assignment's target is a name,
/// not an expression to walk.
pub fn each_child<'a, T: TyRepr>(e: &'a TExpr<T>, f: &mut impl FnMut(&'a TExpr<T>)) {
    match &e.kind {
        TExprKind::Assign(_, value, body) => {
            f(value);
            f(body);
        }
        _ => e.for_each_child(f),
    }
}

/// [`each_child`], mutably.
pub fn each_child_mut<T: TyRepr>(e: &mut TExpr<T>, f: &mut impl FnMut(&mut TExpr<T>)) {
    match &mut e.kind {
        TExprKind::Assign(_, value, body) => {
            f(value);
            f(body);
        }
        _ => e.for_each_child_mut(f),
    }
}

/// Whether any sub-expression of `e` satisfies `test`.
pub fn any_child<T: TyRepr>(e: &TExpr<T>, mut test: impl FnMut(&TExpr<T>) -> bool) -> bool {
    let mut found = false;
    each_child(e, &mut |c| found = found || test(c));
    found
}

/// Whether `name` occurs *free* in `e` — an occurrence under a binder that re-introduces the same
/// name belongs to that binder, not to ours.
pub fn mentions_free<T: TyRepr>(e: &TExpr<T>, name: &str) -> bool {
    match &e.kind {
        TExprKind::Ident(n) => n == name,
        TExprKind::Let(n, value, body) | TExprKind::LetMut(n, value, body) => {
            mentions_free(value, name) || (n != name && mentions_free(body, name))
        }
        TExprKind::For(n, index, iterable, body) => {
            mentions_free(iterable, name)
                || (!aipl_syntax::for_binds(n, index, name) && mentions_free(body, name))
        }
        TExprKind::Match(scrutinee, arms) => {
            mentions_free(scrutinee, name)
                || arms.iter().any(|a| {
                    !a.pattern.bindings().iter().any(|b| b == name) && mentions_free(&a.body, name)
                })
        }
        TExprKind::Lambda(params, body) => {
            !params.iter().any(|p| p.name == name) && mentions_free(body, name)
        }
        _ => any_child(e, |c| mentions_free(c, name)),
    }
}

/// Whether `e` (or any sub-expression) is a *context-typed* literal — `none`, an empty `[]` / `#{}`,
/// or an `ok` / `err` whose other side nothing here decides.
pub(crate) fn contains_context_literal<T: TyRepr>(e: &TExpr<T>) -> bool {
    let here = match &e.kind {
        TExprKind::None => true,
        TExprKind::ArrayLit(v) | TExprKind::SetLit(v, _) => v.is_empty(),
        TExprKind::DictLit(v) => v.is_empty(),
        TExprKind::Call { callee, .. } => matches!(callee, Callee::Ok | Callee::Err),
        _ => false,
    };
    here || any_child(e, contains_context_literal)
}

/// Every name `e` reads. Shadowing is ignored: this only ever adds names.
pub(crate) fn read_names<T: TyRepr>(e: &TExpr<T>, out: &mut std::collections::HashSet<String>) {
    if let TExprKind::Ident(n) = &e.kind {
        out.insert(n.clone());
    }
    each_child(e, &mut |c| read_names(c, out));
}

/// How many times `name` occurs free in an expression, and whether any occurrence sits somewhere
/// that runs other than exactly once. See [`crate::subst::Uses`].
pub(crate) fn uses_of<T: TyRepr>(e: &TExpr<T>, name: &str) -> crate::subst::Uses {
    let mut uses = crate::subst::Uses {
        count: 0,
        repeated: false,
    };
    walk_uses(e, name, false, &mut uses);
    uses
}

fn walk_uses<T: TyRepr>(e: &TExpr<T>, name: &str, repeated: bool, out: &mut crate::subst::Uses) {
    match &e.kind {
        TExprKind::Ident(n) if n == name => {
            out.count += 1;
            out.repeated |= repeated;
        }
        TExprKind::Let(n, value, body) | TExprKind::LetMut(n, value, body) => {
            walk_uses(value, name, repeated, out);
            if n != name {
                walk_uses(body, name, repeated, out);
            }
        }
        TExprKind::For(n, index, iterable, body) => {
            walk_uses(iterable, name, repeated, out);
            if !aipl_syntax::for_binds(n, index, name) {
                walk_uses(body, name, true, out);
            }
        }
        TExprKind::While(cond, body) => {
            walk_uses(cond, name, true, out);
            walk_uses(body, name, true, out);
        }
        TExprKind::Match(scrutinee, arms) => {
            walk_uses(scrutinee, name, repeated, out);
            for a in arms {
                if !a.pattern.bindings().iter().any(|b| b == name) {
                    walk_uses(&a.body, name, repeated, out);
                }
            }
        }
        TExprKind::Lambda(params, body) => {
            if !params.iter().any(|p| p.name == name) {
                walk_uses(body, name, true, out);
            }
        }
        _ => each_child(e, &mut |c| walk_uses(c, name, repeated, out)),
    }
}

/// Every binding written anywhere in `e`: the *root* of each `set` target.
pub(crate) fn assigned_names<T: TyRepr>(e: &TExpr<T>, out: &mut HashSet<String>) {
    fn root_name<T: TyRepr>(e: &TExpr<T>) -> Option<&str> {
        match &e.kind {
            TExprKind::Ident(n) => Some(n),
            TExprKind::Field(x, _) | TExprKind::Index(x, _) | TExprKind::Slice(x, _, _) => {
                root_name(x)
            }
            _ => None,
        }
    }
    if let TExprKind::Assign(lhs, _, _) = &e.kind {
        if let Some(root) = root_name(lhs) {
            out.insert(root.to_string());
        }
    }
    each_child(e, &mut |c| assigned_names(c, out));
}

/// The number of non-leaf expressions in `e`.
pub fn body_size<T: TyRepr>(e: &TExpr<T>) -> usize {
    let leaf = matches!(
        e.kind,
        TExprKind::Int(_)
            | TExprKind::Float(_)
            | TExprKind::Bool(_)
            | TExprKind::Str(_)
            | TExprKind::Char(_)
            | TExprKind::Ident(_)
            | TExprKind::None
            | TExprKind::Unit
    );
    let mut size = usize::from(!leaf);
    each_child(e, &mut |c| size += body_size(c));
    size
}

/// `e` with every binder it introduces renamed, for a copy of code that already has another copy in
/// the same function.
pub(crate) fn distinct_binders<T: TyRepr>(e: &TExpr<T>) -> TExpr<T> {
    rename_scoped(e, &HashMap::new(), true)
}

/// [`aipl_syntax::rename_scoped`] over the typed tree.
pub(crate) fn rename_scoped<T: TyRepr>(
    e: &TExpr<T>,
    map: &HashMap<String, String>,
    fresh: bool,
) -> TExpr<T> {
    let sub = |n: &String| map.get(n).cloned().unwrap_or_else(|| n.clone());
    let scope = |names: &[String]| -> (HashMap<String, String>, HashMap<String, String>) {
        let mut inner = map.clone();
        let mut renamed = HashMap::new();
        for n in names {
            inner.remove(n);
            if fresh && n != "_" {
                let nm = match n.strip_prefix(aipl_syntax::SHIM_CLOSURE_PREFIX) {
                    Some(rest) => format!(
                        "{}$inl{}_{rest}",
                        aipl_syntax::SHIM_CLOSURE_PREFIX,
                        aipl_syntax::fresh_id()
                    ),
                    None => format!("$inl{}_{}", aipl_syntax::fresh_id(), n),
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
    let boxed = |x: &TExpr<T>, m: &HashMap<String, String>| Box::new(rename_scoped(x, m, fresh));
    let arm = |a: &TMatchArm<T>| {
        let (inner, renamed) = scope(&a.pattern.bindings());
        TMatchArm {
            pattern: a
                .pattern
                .rename_bindings(&mut |n| renamed.get(n).cloned().unwrap_or_else(|| n.to_string())),
            body: rename_scoped(&a.body, &inner, fresh),
            span: a.span.clone(),
        }
    };
    let kind = match &e.kind {
        TExprKind::Shim(effect, bindings, body) => TExprKind::Shim(
            effect.clone(),
            bindings
                .iter()
                .map(|(op, f)| (op.clone(), sub(f)))
                .collect(),
            boxed(body, map),
        ),
        TExprKind::Ident(n) => TExprKind::Ident(sub(n)),
        TExprKind::Call {
            callee,
            type_args,
            args,
            method_style,
        } => TExprKind::Call {
            callee: match callee {
                Callee::Local(n) => Callee::Local(sub(n)),
                known => known.clone(),
            },
            type_args: type_args.clone(),
            args: args.iter().map(|a| rename_scoped(a, map, fresh)).collect(),
            method_style: *method_style,
        },
        TExprKind::Assign(lhs, a, b) => {
            TExprKind::Assign(boxed(lhs, map), boxed(a, map), boxed(b, map))
        }
        TExprKind::Let(n, a, b) => {
            let (inner, renamed) = scope(std::slice::from_ref(n));
            TExprKind::Let(known_as(&renamed, n), boxed(a, map), boxed(b, &inner))
        }
        TExprKind::LetMut(n, a, b) => {
            let (inner, renamed) = scope(std::slice::from_ref(n));
            TExprKind::LetMut(known_as(&renamed, n), boxed(a, map), boxed(b, &inner))
        }
        TExprKind::For(v, index, iter, b) => {
            let (inner, renamed) = scope(&aipl_syntax::loop_binders(v, index));
            TExprKind::For(
                known_as(&renamed, v),
                index.as_ref().map(|i| known_as(&renamed, i)),
                boxed(iter, map),
                boxed(b, &inner),
            )
        }
        TExprKind::Match(s, arms) => {
            TExprKind::Match(boxed(s, map), arms.iter().map(arm).collect())
        }
        TExprKind::IfLet(a, s, else_b) => {
            TExprKind::IfLet(Box::new(arm(a)), boxed(s, map), boxed(else_b, map))
        }
        TExprKind::Lambda(ps, b) => {
            let names: Vec<String> = ps.iter().map(|p| p.name.clone()).collect();
            let (inner, renamed) = scope(&names);
            let ps = ps
                .iter()
                .map(|p| aipl_syntax::typed::TLambdaParam {
                    name: known_as(&renamed, &p.name),
                    ..p.clone()
                })
                .collect();
            TExprKind::Lambda(ps, boxed(b, &inner))
        }
        _ => {
            // Nothing here binds or names: the sub-expressions are renamed as they stand.
            let mut out = e.clone();
            each_child_mut(&mut out, &mut |c| *c = rename_scoped(c, map, fresh));
            return out;
        }
    };
    let mut out = TExpr::rebuilt(kind, e.ty.clone(), e);
    out.annotation = e.annotation.clone();
    out
}

/// Count each global name's uses in `e`: a mention, a call, or a shim binding of a name not in
/// `bound`, which holds the locals in scope (shadowing a global hides it).
pub(crate) fn count_uses<'a, T: TyRepr>(
    e: &'a TExpr<T>,
    bound: &mut HashSet<&'a str>,
    counts: &mut HashMap<&'a str, usize>,
) {
    let arm_bindings = |pattern: &'a aipl_syntax::typed::TPattern<T>,
                        bound: &mut HashSet<&'a str>| {
        let mut added: Vec<&'a str> = Vec::new();
        pattern.each_binding(&mut |b| {
            if bound.insert(b) {
                added.push(b);
            }
        });
        added
    };
    match &e.kind {
        TExprKind::Shim(_, bindings, body) => {
            for (_, f) in bindings {
                if !bound.contains(f.as_str()) {
                    *counts.entry(f).or_insert(0) += 1;
                }
            }
            count_uses(body, bound, counts);
        }
        TExprKind::Ident(n) => {
            if !bound.contains(n.as_str()) {
                *counts.entry(n).or_insert(0) += 1;
            }
        }
        TExprKind::Call { callee, args, .. } => {
            let name = callee.name();
            if !bound.contains(name) {
                *counts.entry(name).or_insert(0) += 1;
            }
            for a in args {
                count_uses(a, bound, counts);
            }
        }
        TExprKind::Match(s, arms) => {
            count_uses(s, bound, counts);
            for arm in arms {
                let added = arm_bindings(&arm.pattern, bound);
                count_uses(&arm.body, bound, counts);
                for b in added {
                    bound.remove(b);
                }
            }
        }
        TExprKind::IfLet(arm, s, else_b) => {
            count_uses(s, bound, counts);
            let added = arm_bindings(&arm.pattern, bound);
            count_uses(&arm.body, bound, counts);
            for b in added {
                bound.remove(b);
            }
            count_uses(else_b, bound, counts);
        }
        TExprKind::Let(n, val, b) | TExprKind::LetMut(n, val, b) => {
            count_uses(val, bound, counts);
            let added = bound.insert(n);
            count_uses(b, bound, counts);
            if added {
                bound.remove(n.as_str());
            }
        }
        TExprKind::For(v, index, iter, b) => {
            count_uses(iter, bound, counts);
            let added = bound.insert(v);
            let added_index = index.as_ref().is_some_and(|i| bound.insert(i));
            count_uses(b, bound, counts);
            if added_index {
                bound.remove(index.as_ref().expect("inserted above").as_str());
            }
            if added {
                bound.remove(v.as_str());
            }
        }
        TExprKind::Lambda(ps, b) => {
            let added: Vec<&str> = ps
                .iter()
                .filter_map(|p| bound.insert(&p.name).then_some(p.name.as_str()))
                .collect();
            count_uses(b, bound, counts);
            for p in added {
                bound.remove(p);
            }
        }
        _ => each_child(e, &mut |c| count_uses(c, bound, counts)),
    }
}

/// Every name a `let`, `mut`, `for`, lambda parameter or `match` arm anywhere in `e` binds.
pub(crate) fn collect_body_binders<'a, T: TyRepr>(e: &'a TExpr<T>, set: &mut HashSet<&'a str>) {
    match &e.kind {
        TExprKind::Let(n, _, _) | TExprKind::LetMut(n, _, _) => {
            set.insert(n);
        }
        TExprKind::For(n, index, _, _) => {
            set.insert(n);
            if let Some(i) = index {
                set.insert(i);
            }
        }
        TExprKind::Lambda(ps, _) => {
            for p in ps {
                set.insert(&p.name);
            }
        }
        TExprKind::Match(_, arms) => {
            for a in arms {
                a.pattern.each_binding(&mut |b| {
                    set.insert(b);
                });
            }
        }
        _ => {}
    }
    each_child(e, &mut |c| collect_body_binders(c, set));
}

/// Whether `e` (or any sub-expression) calls an in-place higher-order intrinsic: one of the calls
/// the monomorphizer synthesizes for the loop bodies of `map`, `filter` and `zip_with`.
pub(crate) fn contains_inplace_hof_intrinsic<T: TyRepr>(e: &TExpr<T>) -> bool {
    if let TExprKind::Call { callee, .. } = &e.kind {
        if matches!(
            callee,
            Callee::ArrWritable
                | Callee::MapSet
                | Callee::MapResult
                | Callee::FilterDrop
                | Callee::FilterKeep
                | Callee::FilterTruncate
        ) {
            return true;
        }
    }
    any_child(e, contains_inplace_hof_intrinsic)
}

/// Whether `e` references `name` as a callee or as a value. Only asked of a `name` that is never a
/// binder, so there is no shadowing to consider.
pub(crate) fn references_name<T: TyRepr>(e: &TExpr<T>, name: &str) -> bool {
    match &e.kind {
        TExprKind::Ident(n) => n == name,
        TExprKind::Call { callee, args, .. } => {
            callee == name || args.iter().any(|a| references_name(a, name))
        }
        _ => any_child(e, |c| references_name(c, name)),
    }
}
