//! A `match` arm that rebuilds the value it just took apart: `Term(k) => Term(k)`.
//!
//! Variants are values, so a case rebuilt from exactly the payloads it was
//! matched with *is* the scrutinee. Building it again allocates a block (a boxed
//! variant), retains every payload, and drops the original later — all to
//! produce a value that already exists. A tree rewrite that changes some cases
//! and passes the rest through (`desugar_rule`, `link_rule`) pays that for every
//! leaf it copies, which is most of them:
//!
//! ```text
//! match (r) { Term(k) => ok(Term(k)), Then(rs) => .. }   →   match (r) { Term(k) => ok(r), .. }
//! ```
//!
//! # What is matched
//!
//! The scrutinee is a name, and one the function was handed: an immutable
//! parameter, whose type is therefore known, and which no binder in the body
//! shadows. The arm's pattern names a case and binds all of its payload slots
//! (a `Case(..)` pattern, or a binder named `_`, has no value to rebuild from).
//! The rebuild is a call to *that case of that variant instance* —
//! `Case@Instance`, the instance being the parameter's own type — applied to
//! exactly the arm's binders, in order, or the bare reference to a case with no
//! payload.
//!
//! The qualifier is what makes it sound: a different variant with a case of the
//! same name and the same payload types would not be the scrutinee's value.
//!
//! # When it is refused
//!
//! The replacement reads the scrutinee name inside the arm, so the arm may not
//! shadow it or any of its binders (the rebuild would then read a different
//! value than the one matched), and may not assign the scrutinee — a name
//! assigned in the arm is a `mut` one, and the binders were read from its value
//! at the match.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{Callee, ConcreteType};
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TPattern};

use crate::passes::Scope;
use crate::typed_util::{assigned_names, each_child, each_child_mut, Typed};
use crate::{ConcreteFn, MonoProgram};

/// `program` with, in each function `scope` covers, every construction of a case a parameter was
/// just matched as replaced by that parameter.
pub fn reuse_matched_values(program: &MonoProgram, scope: &Scope) -> MonoProgram {
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| match scope.covers(&f.name) {
                true => rewritten(f),
                false => f.clone(),
            })
            .collect(),
        ..program.clone()
    }
}

fn rewritten(f: &ConcreteFn) -> ConcreteFn {
    let mut shadowed = HashSet::new();
    bound_names(&f.body, &mut shadowed);
    let params: HashMap<&str, &str> = f
        .params
        .iter()
        .filter(|p| !p.mutable && !shadowed.contains(&p.name))
        .filter_map(|p| match &p.ty {
            ConcreteType::Named(instance) => Some((p.name.as_str(), instance.as_str())),
            _ => None,
        })
        .collect();
    if params.is_empty() {
        return f.clone();
    }
    let mut body = f.body.clone();
    visit(&mut body, &params);
    ConcreteFn { body, ..f.clone() }
}

fn bound_names(e: &Typed, out: &mut HashSet<String>) {
    match &e.kind {
        TExprKind::Let(n, _, _) | TExprKind::LetMut(n, _, _) => {
            out.insert(n.clone());
        }
        TExprKind::For(n, index, _, _) => {
            out.insert(n.clone());
            out.extend(index.iter().cloned());
        }
        TExprKind::Lambda(params, _) => out.extend(params.iter().map(|p| p.name.clone())),
        TExprKind::Match(_, arms) => {
            for a in arms {
                out.extend(a.pattern.bindings());
            }
        }
        TExprKind::IfLet(arm, _, _) => out.extend(arm.pattern.bindings()),
        _ => {}
    }
    each_child(e, &mut |c| bound_names(c, out));
}

fn visit(e: &mut Typed, params: &HashMap<&str, &str>) {
    each_child_mut(e, &mut |child| visit(child, params));
    let TExprKind::Match(scrutinee, arms) = &mut e.kind else {
        return;
    };
    let TExprKind::Ident(name) = &scrutinee.kind else {
        return;
    };
    let Some(&instance) = params.get(name.as_str()) else {
        return;
    };
    let name = name.clone();
    for arm in arms.iter_mut() {
        reuse_in_arm(arm, &name, instance);
    }
}

fn reuse_in_arm(arm: &mut TMatchArm<ConcreteType>, name: &str, instance: &str) {
    let TPattern::Ctor {
        name: case,
        bindings,
        ignore_payload: false,
    } = &arm.pattern
    else {
        return;
    };
    if bindings.iter().any(|b| b == "_") {
        return;
    }
    let callee = format!("{}@{instance}", case.split('@').next().unwrap_or(case));
    let mut bound = HashSet::new();
    bound_names(&arm.body, &mut bound);
    let mut assigned = HashSet::new();
    assigned_names(&arm.body, &mut assigned);
    if assigned.contains(name) || bound.contains(name) || bindings.iter().any(|b| bound.contains(b))
    {
        return;
    }
    replace(&mut arm.body, &callee, bindings, name);
}

fn replace(e: &mut Typed, callee: &str, bindings: &[String], name: &str) {
    each_child_mut(e, &mut |child| replace(child, callee, bindings, name));
    let rebuilds = match &e.kind {
        TExprKind::Call {
            callee: Callee::User(c),
            args,
            method_style: false,
            ..
        } if c == callee => {
            args.len() == bindings.len()
                && args
                    .iter()
                    .zip(bindings)
                    .all(|(a, b)| matches!(&a.kind, TExprKind::Ident(n) if n == b))
        }
        TExprKind::Ident(c) => c == callee && bindings.is_empty(),
        _ => false,
    };
    if rebuilds {
        *e = TExpr::rebuilt(TExprKind::Ident(name.to_string()), e.ty.clone(), e);
    }
}
