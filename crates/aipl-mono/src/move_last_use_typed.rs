//! The final marking of last uses, on the typed tree: [`crate::move_last_use`]'s analysis for the
//! program after monomorphization, which is the only place it runs on typed bodies. The module
//! docs there say what a mark is and why the early marks are stripped and made again; this is
//! the same walk over `TExpr<ConcreteTy>`.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, ConcreteType};
use aipl_syntax::typed::{ConcreteTy, TExpr, TExprKind, TMatchArm};

use crate::typed_util::{any_child, each_child, each_child_mut};
use crate::{is_heap_concrete, MonoProgram};

type Typed = TExpr<ConcreteTy>;

fn placeholder(like: &Typed) -> Typed {
    TExpr::new(TExprKind::Unit, ConcreteTy::Unit, like.span.clone())
}

/// `inner` handed over: `__move(inner)`, which is the value `inner` is and so has its type.
fn moved(inner: Typed) -> Typed {
    let mut node = TExpr::new(
        TExprKind::Call {
            callee: Callee::Move,
            type_args: Vec::new(),
            args: vec![inner.clone()],
            method_style: false,
        },
        inner.ty.clone(),
        inner.span.clone(),
    );
    node.derived = inner.derived;
    node
}

fn count_ident(name: &str, e: &Typed) -> usize {
    let c = |x: &Typed| count_ident(name, x);
    match &e.kind {
        TExprKind::Ident(n) => usize::from(n == name),
        // A bare-ident target is an lvalue, not a read; a field-path target also *reads* its base.
        TExprKind::Assign(lhs, a, b) => {
            let lhs_reads = if matches!(&lhs.kind, TExprKind::Ident(_)) {
                0
            } else {
                c(lhs)
            };
            lhs_reads + c(a) + c(b)
        }
        _ => {
            let mut n = 0;
            each_child(e, &mut |x| n += c(x));
            n
        }
    }
}

fn mentions(name: &str, e: &Typed) -> bool {
    count_ident(name, e) > 0
}

fn is_name(e: &Typed, name: &str) -> bool {
    matches!(&e.kind, TExprKind::Ident(n) if n == name)
}

fn binds(arm: &TMatchArm<ConcreteTy>, name: &str) -> bool {
    arm.pattern.bindings().iter().any(|b| b == name)
}

/// Whether `body` uses `param` only as `param.f` reads, with no field read twice, in a loop, or in
/// a lambda.
fn fields_read_once(param: &str, body: &Typed) -> bool {
    let mut seen: Vec<String> = Vec::new();
    fields_read_once_into(&[param], body, false, &mut seen)
}

fn fields_read_once_into(names: &[&str], e: &Typed, repeats: bool, seen: &mut Vec<String>) -> bool {
    let is_alias =
        |x: &Typed| matches!(&x.kind, TExprKind::Ident(n) if names.contains(&n.as_str()));
    match &e.kind {
        TExprKind::Field(obj, f) if is_alias(obj) => {
            if repeats || seen.contains(f) {
                return false;
            }
            seen.push(f.clone());
            true
        }
        TExprKind::Ident(n) => !names.contains(&n.as_str()),
        TExprKind::Let(a, value, body) | TExprKind::LetMut(a, value, body)
            if is_alias(value) && !names.contains(&a.as_str()) =>
        {
            let mut inner: Vec<&str> = names.to_vec();
            inner.push(a);
            fields_read_once_into(&inner, body, repeats, seen)
        }
        TExprKind::For(_, _, iter, fbody) => {
            fields_read_once_into(names, iter, repeats, seen)
                && fields_read_once_into(names, fbody, true, seen)
        }
        TExprKind::While(cond, wbody) => {
            fields_read_once_into(names, cond, true, seen)
                && fields_read_once_into(names, wbody, true, seen)
        }
        TExprKind::Lambda(_, lbody) => fields_read_once_into(names, lbody, true, seen),
        _ => {
            let mut ok = true;
            each_child(e, &mut |c| {
                ok = ok && fields_read_once_into(names, c, repeats, seen)
            });
            ok
        }
    }
}

/// `body` — the scope of the *parameter* `name`, which the instance owns — with each last use
/// wrapped in `__move`.
fn move_last_uses_of_param(
    name: &str,
    body: &Typed,
    heap_fields: &HashSet<String>,
    mutating: &HashSet<String>,
) -> Typed {
    let no_value = placeholder(body);
    move_last_uses(name, &no_value, body, heap_fields, mutating)
}

/// `body` — the scope of the binding `name`, whose initializer is `value` — with each last use of
/// `name` that is an argument to a user function wrapped in `__move`.
fn move_last_uses(
    name: &str,
    value: &Typed,
    body: &Typed,
    heap_fields: &HashSet<String>,
    mutating: &HashSet<String>,
) -> Typed {
    if matches!(&value.kind, TExprKind::Ident(_)) || !forwarded(name, body, true, heap_fields) {
        return body.clone();
    }
    let mut out = body.clone();
    let mut alias_outlives = false;
    mark(name, &mut out, true, mutating, &mut alias_outlives);
    if alias_outlives {
        return body.clone();
    }
    out
}

/// `body` with each read of one of the struct binding `name`'s `heap_fields` wrapped in `__move`,
/// when the binding owns its value and reads each such field at most once and never in a loop.
fn move_field_reads(
    name: &str,
    value: &Typed,
    body: &Typed,
    heap_fields: &HashSet<String>,
) -> Typed {
    if heap_fields.is_empty()
        || matches!(&value.kind, TExprKind::Ident(_))
        || !fields_read_once(name, body)
    {
        return body.clone();
    }
    let mut out = body.clone();
    mark_fields(name, &mut out, heap_fields);
    out
}

fn mark_fields(name: &str, e: &mut Typed, heap_fields: &HashSet<String>) {
    let take = matches!(&e.kind, TExprKind::Field(obj, f) if is_name(obj, name) && heap_fields.contains(f));
    if take {
        let hole = placeholder(e);
        let inner = std::mem::replace(e, hole);
        *e = moved(inner);
        return;
    }
    each_child_mut(e, &mut |child| mark_fields(name, child, heap_fields));
}

/// The final marking, over the whole program: every `__move` the early run left is stripped and
/// the analysis re-run on each binding.
pub fn move_last_uses_post_mono(program: &MonoProgram) -> MonoProgram {
    let mutating: HashSet<String> = program
        .fns
        .iter()
        .filter(|f| f.is_mutating())
        .map(|f| f.name.clone())
        .collect();
    // The instances that take their receiver rather than borrowing it.
    let owns_receiver: HashSet<String> = program
        .fns
        .iter()
        .filter(|f| f.is_mutating() && f.params.first().is_some_and(|p| p.owned))
        .map(|f| f.name.clone())
        .collect();
    let by_name = |name: &str| program.fns.iter().find(|f| f.name == name);
    crate::typed_adapter::on_typed_named(program, |name, body| {
        let f = by_name(name).expect("a function of the program");
        let marked = mark_owned_params(
            f,
            program,
            &remark(&strip(body), program, &mutating),
            &mutating,
        );
        *body = move_owned_receivers(&marked, &owns_receiver);
    })
}

fn mark_owned_params(
    f: &crate::ConcreteFn,
    program: &MonoProgram,
    body: &Typed,
    mutating: &HashSet<String>,
) -> Typed {
    let mut out = body.clone();
    for p in f.params.iter().filter(|p| p.owned && !p.mutable) {
        let heap_fields = heap_fields_of_concrete(program, &p.ty);
        if heap_fields.is_empty() || is_heap_concrete(&p.ty) {
            continue;
        }
        out = move_last_uses_of_param(&p.name, &out, &heap_fields, mutating);
    }
    marks_of_moved_aliases(&out, program, mutating)
}

fn heap_fields_of_concrete(program: &MonoProgram, ty: &ConcreteType) -> HashSet<String> {
    let ConcreteType::Named(name) = ty else {
        return HashSet::new();
    };
    heap_fields_named(program, name)
}

fn heap_fields_named(program: &MonoProgram, name: &str) -> HashSet<String> {
    program
        .structs
        .iter()
        .find(|d| d.name == name)
        .map(|d| {
            d.fields
                .iter()
                .filter(|f| owns_heap_post_mono(program, &f.ty))
                .map(|f| f.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The heap-owning fields of the struct a binding is annotated as, from the finished program's
/// declarations — empty for anything but a struct.
fn heap_fields_annotated(
    program: &MonoProgram,
    annotation: Option<&ConcreteTy>,
) -> HashSet<String> {
    match annotation {
        Some(ConcreteTy::Named(name)) => heap_fields_named(program, name),
        _ => HashSet::new(),
    }
}

fn owns_heap_post_mono(program: &MonoProgram, ty: &ConcreteType) -> bool {
    if is_heap_concrete(ty) {
        return true;
    }
    let ConcreteType::Named(name) = ty else {
        return false;
    };
    program
        .structs
        .iter()
        .find(|d| &d.name == name)
        .is_some_and(|d| d.fields.iter().any(|f| owns_heap_post_mono(program, &f.ty)))
}

/// Hand the receiver over at every call to a mutating method that owns it.
fn move_owned_receivers(e: &Typed, owns_receiver: &HashSet<String>) -> Typed {
    let mut out = e.clone();
    if let TExprKind::Call {
        callee: Callee::User(f),
        args,
        method_style: true,
        ..
    } = &mut out.kind
    {
        if owns_receiver.contains(f) {
            if let Some(receiver) = args.first_mut() {
                // A bare name only: anything else is not the writeback shape.
                if matches!(receiver.kind, TExprKind::Ident(_)) {
                    let hole = placeholder(receiver);
                    let inner = std::mem::replace(receiver, hole);
                    *receiver = moved(inner);
                }
            }
        }
    }
    each_child_mut(&mut out, &mut |child| {
        *child = move_owned_receivers(child, owns_receiver);
    });
    out
}

/// `e` with every `__move(x)` back to `x`.
fn strip(e: &Typed) -> Typed {
    if let TExprKind::Call {
        callee: Callee::Move,
        args,
        ..
    } = &e.kind
    {
        if let [inner] = args.as_slice() {
            return strip(inner);
        }
    }
    let mut out = e.clone();
    each_child_mut(&mut out, &mut |child| *child = strip(child));
    out
}

/// Moves for the aliases this binding's marking just turned into owners.
fn marks_of_moved_aliases(e: &Typed, program: &MonoProgram, mutating: &HashSet<String>) -> Typed {
    let mut out = e.clone();
    each_child_mut(&mut out, &mut |child| {
        *child = marks_of_moved_aliases(child, program, mutating);
    });
    let (name, value, body) = match &out.kind {
        TExprKind::Let(n, v, b) | TExprKind::LetMut(n, v, b) => (n, v, b),
        _ => return out,
    };
    let moved_alias = matches!(&value.kind,
        TExprKind::Call { callee: Callee::Move, args, .. } if matches!(args.as_slice(), [a] if matches!(a.kind, TExprKind::Ident(_))));
    if !moved_alias || already_moves(name, body) {
        return out;
    }
    let heap_fields = heap_fields_annotated(program, out.annotation.as_ref());
    let new_body = move_last_uses(name, value, body, &heap_fields, mutating);
    let new_body = move_field_reads(name, value, &new_body, &heap_fields);
    match &mut out.kind {
        TExprKind::Let(_, _, b) | TExprKind::LetMut(_, _, b) => *b = Box::new(new_body),
        _ => unreachable!(),
    }
    out
}

/// Whether `body` already contains `__move(name)` or `__move(name.f)`.
fn already_moves(name: &str, body: &Typed) -> bool {
    if let TExprKind::Call {
        callee: Callee::Move,
        args,
        ..
    } = &body.kind
    {
        if matches!(args.as_slice(), [a] if is_name(a, name)
            || matches!(&a.kind, TExprKind::Field(obj, _) if is_name(obj, name)))
        {
            return true;
        }
    }
    any_child(body, |c| already_moves(name, c))
}

/// `e` with every binding under it marked, innermost first.
fn remark(e: &Typed, program: &MonoProgram, mutating: &HashSet<String>) -> Typed {
    let mut out = e.clone();
    each_child_mut(&mut out, &mut |child| {
        *child = remark(child, program, mutating)
    });
    match &out.kind {
        TExprKind::Let(name, value, body) | TExprKind::LetMut(name, value, body) => {
            let heap_fields = heap_fields_annotated(program, out.annotation.as_ref());
            let body = move_last_uses(name, value, body, &heap_fields, mutating);
            let body = move_field_reads(name, value, &body, &heap_fields);
            let body = marks_of_moved_aliases(&body, program, mutating);
            let (n, v) = (name.clone(), value.clone());
            out.kind = match &out.kind {
                TExprKind::Let(..) => TExprKind::Let(n, v, Box::new(body)),
                _ => TExprKind::LetMut(n, v, Box::new(body)),
            };
            out
        }
        _ => out,
    }
}

/// Whether every occurrence of `name` in `e` is a direct call argument — or, for a struct, a read
/// of one of its fields outside `heap_fields` — with none under a lambda and no rebinding of the
/// name. `as_argument` says whether `e` itself sits in argument position.
fn only_call_arguments(
    name: &str,
    e: &Typed,
    as_argument: bool,
    heap_fields: &HashSet<String>,
) -> bool {
    let all = |es: &[&Typed]| {
        es.iter()
            .all(|c| only_call_arguments(name, c, false, heap_fields))
    };
    match &e.kind {
        TExprKind::Ident(n) => n != name || as_argument,
        TExprKind::Field(obj, f) if is_name(obj, name) => !heap_fields.contains(f),
        TExprKind::Let(n, value, body) | TExprKind::LetMut(n, value, body)
            if n != name && is_name(value, name) =>
        {
            only_call_arguments(name, body, false, heap_fields)
        }
        TExprKind::Call { args, .. } => args
            .iter()
            .all(|a| only_call_arguments(name, a, true, heap_fields)),
        TExprKind::Lambda(params, body) => {
            params.iter().all(|p| p.name != name) && !mentions(name, body)
        }
        TExprKind::Let(n, _, _) | TExprKind::LetMut(n, _, _) if n == name => false,
        TExprKind::For(n, index, _, _) if aipl_syntax::for_binds(n, index, name) => false,
        TExprKind::Assign(lhs, value, rest) => {
            (is_name(lhs, name) || only_call_arguments(name, lhs, false, heap_fields))
                && all(&[value, rest])
        }
        TExprKind::Match(scrutinee, arms) => {
            arms.iter().all(|a| !binds(a, name))
                && only_call_arguments(name, scrutinee, false, heap_fields)
                && arms
                    .iter()
                    .all(|a| only_call_arguments(name, &a.body, false, heap_fields))
        }
        TExprKind::IfLet(arm, scrutinee, else_body) => {
            !binds(arm, name) && all(&[scrutinee, &arm.body, else_body])
        }
        _ => {
            let mut ok = true;
            each_child(e, &mut |c| {
                ok = ok && only_call_arguments(name, c, false, heap_fields)
            });
            ok
        }
    }
}

fn forwarded(name: &str, e: &Typed, tail: bool, heap_fields: &HashSet<String>) -> bool {
    let rec = |x: &Typed| forwarded(name, x, false, heap_fields);
    let rec_tail = |x: &Typed| forwarded(name, x, tail, heap_fields);
    match &e.kind {
        TExprKind::Ident(n) if n == name => tail,
        TExprKind::Let(n, value, body) | TExprKind::LetMut(n, value, body) if n != name => {
            (is_name(value, name) || rec(value)) && rec_tail(body)
        }
        TExprKind::Seq(first, rest) => rec(first) && rec_tail(rest),
        TExprKind::Assign(lhs, value, rest) => {
            (is_name(lhs, name) || only_call_arguments(name, lhs, false, heap_fields))
                && rec(value)
                && rec_tail(rest)
        }
        TExprKind::If(cond, then_body, else_body) => {
            rec(cond) && rec_tail(then_body) && rec_tail(else_body)
        }
        TExprKind::Match(scrutinee, arms) => {
            arms.iter().all(|a| !binds(a, name))
                && rec(scrutinee)
                && arms.iter().all(|a| rec_tail(&a.body))
        }
        TExprKind::Return(value) => forwarded(name, value, true, heap_fields),
        TExprKind::Construct(..)
        | TExprKind::TupleLit(..)
        | TExprKind::ArrayLit(..)
        | TExprKind::SetLit(..)
        | TExprKind::DictLit(..)
        | TExprKind::Call { .. }
            if tail && count_ident(name, e) == 1 =>
        {
            let call = matches!(&e.kind, TExprKind::Call { .. });
            let mut ok = true;
            each_child(e, &mut |part| {
                ok = ok
                    && if is_name(part, name) {
                        true
                    } else if is_composite_literal(part) {
                        forwarded(name, part, true, heap_fields)
                    } else if call {
                        only_call_arguments(name, part, true, heap_fields)
                    } else {
                        forwarded(name, part, true, heap_fields)
                    };
            });
            ok
        }
        _ => only_call_arguments(name, e, false, heap_fields),
    }
}

fn is_composite_literal(e: &Typed) -> bool {
    matches!(
        &e.kind,
        TExprKind::Construct(..)
            | TExprKind::TupleLit(..)
            | TExprKind::ArrayLit(..)
            | TExprKind::SetLit(..)
            | TExprKind::DictLit(..)
    )
}

/// Whether `e` certainly evaluates to a scalar and so cannot hold a reference to anything it was
/// computed from. Conservative.
fn scalar_valued(e: &Typed) -> bool {
    match &e.kind {
        TExprKind::Bool(_)
        | TExprKind::Int(_)
        | TExprKind::Float(_)
        | TExprKind::Char(_)
        | TExprKind::Neg(_) => true,
        TExprKind::Call { callee, .. } => {
            aipl_syntax::binop_for_builtin(callee).is_some()
                || matches!(
                    callee,
                    Callee::LogicalNot | Callee::Len | Callee::IsEmpty | Callee::IsNonempty
                )
        }
        TExprKind::If(_, then_body, else_body) => {
            scalar_valued(then_body) && scalar_valued(else_body)
        }
        TExprKind::Match(_, arms) => {
            !arms.is_empty() && arms.iter().all(|a| scalar_valued(&a.body))
        }
        TExprKind::Let(_, _, body) | TExprKind::LetMut(_, _, body) => scalar_valued(body),
        TExprKind::Seq(_, rest) => scalar_valued(rest),
        _ => false,
    }
}

/// Wrap each last-use argument of `name` under `e`. `tail` is whether the binding is dead once `e`
/// finishes. `alias_outlives` is set when a `let a = name` is found that is not `name`'s last use.
fn mark(
    name: &str,
    e: &mut Typed,
    tail: bool,
    mutating: &HashSet<String>,
    alias_outlives: &mut bool,
) {
    match &mut e.kind {
        TExprKind::Let(_, value, body) | TExprKind::LetMut(_, value, body)
            if is_name(value, name) =>
        {
            if tail && !mentions(name, body) {
                let hole = placeholder(value);
                let inner = std::mem::replace(value.as_mut(), hole);
                *value = Box::new(moved(inner));
            } else if !scalar_valued(body) {
                *alias_outlives = true;
            }
            mark(name, body, tail, mutating, alias_outlives);
        }
        TExprKind::Call { callee, args, .. } => {
            let user =
                matches!(callee, Callee::User(f) | Callee::Local(f) if !mutating.contains(f));
            let takes_array = matches!(callee, Callee::With | Callee::WithOrSelf);
            let later: Vec<bool> = (0..args.len())
                .map(|i| args[i + 1..].iter().any(|a| mentions(name, a)))
                .collect();
            let earlier: Vec<bool> = (0..args.len())
                .map(|i| args[..i].iter().any(|a| mentions(name, a)))
                .collect();
            for (position, ((arg, later), earlier)) in
                args.iter_mut().zip(later).zip(earlier).enumerate()
            {
                let t = tail && !later && !earlier;
                if (user || (takes_array && position == 0)) && t && is_name(arg, name) {
                    let hole = placeholder(arg);
                    let inner = std::mem::replace(arg, hole);
                    *arg = moved(inner);
                } else {
                    mark(name, arg, t, mutating, alias_outlives);
                }
            }
        }
        TExprKind::Seq(first, rest) => {
            let t = tail && !mentions(name, rest);
            mark(name, first, t, mutating, alias_outlives);
            mark(name, rest, tail, mutating, alias_outlives);
        }
        TExprKind::Let(_, value, body) | TExprKind::LetMut(_, value, body) => {
            let t = tail && !mentions(name, body);
            mark(name, value, t, mutating, alias_outlives);
            mark(name, body, tail, mutating, alias_outlives);
        }
        TExprKind::Assign(lhs, value, rest) => {
            let t = if is_name(lhs, name) {
                true
            } else {
                tail && !mentions(name, rest)
            };
            mark(name, value, t, mutating, alias_outlives);
            mark(name, rest, tail, mutating, alias_outlives);
        }
        TExprKind::If(cond, then_body, else_body) => {
            let t = tail && !mentions(name, then_body) && !mentions(name, else_body);
            // An alias made inside the condition is gone before either branch runs.
            let mut dies_with_condition = false;
            mark(name, cond, t, mutating, &mut dies_with_condition);
            mark(name, then_body, tail, mutating, alias_outlives);
            mark(name, else_body, tail, mutating, alias_outlives);
        }
        TExprKind::IfLet(arm, scrutinee, else_body) => {
            let t = tail && !mentions(name, &arm.body) && !mentions(name, else_body);
            mark(name, scrutinee, t, mutating, alias_outlives);
            mark(name, &mut arm.body, tail, mutating, alias_outlives);
            mark(name, else_body, tail, mutating, alias_outlives);
        }
        TExprKind::Match(scrutinee, arms) => {
            let t = tail && arms.iter().all(|a| !mentions(name, &a.body));
            mark(name, scrutinee, t, mutating, alias_outlives);
            for arm in arms {
                mark(name, &mut arm.body, tail, mutating, alias_outlives);
            }
        }
        TExprKind::For(_, _, iter, body) => {
            let t = tail && !mentions(name, body);
            mark(name, iter, t, mutating, alias_outlives);
            mark(name, body, false, mutating, alias_outlives);
        }
        TExprKind::While(cond, body) => {
            mark(name, cond, false, mutating, alias_outlives);
            mark(name, body, false, mutating, alias_outlives);
        }
        TExprKind::Return(value) => mark(name, value, true, mutating, alias_outlives),
        TExprKind::Lambda(..) => {}
        _ => {
            let later: Vec<bool> = {
                let mut kids: Vec<&Typed> = Vec::new();
                each_child(e, &mut |c| kids.push(c));
                (0..kids.len())
                    .map(|i| kids[i + 1..].iter().any(|c| mentions(name, c)))
                    .collect()
            };
            let mut i = 0;
            each_child_mut(e, &mut |child| {
                mark(name, child, tail && !later[i], mutating, alias_outlives);
                i += 1;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: TExprKind<ConcreteTy>) -> Typed {
        TExpr::new(kind, ConcreteTy::Unit, 0..0)
    }

    fn id(n: &str) -> Typed {
        e(TExprKind::Ident(n.to_string()))
    }

    fn call(callee: Callee, args: Vec<Typed>) -> Typed {
        e(TExprKind::Call {
            callee,
            type_args: Vec::new(),
            args,
            method_style: false,
        })
    }

    #[test]
    fn stripping_undoes_marking() {
        let f = |x: Typed| call(Callee::User("f".into()), vec![x]);
        let body = e(TExprKind::Seq(Box::new(f(id("x"))), Box::new(f(id("x")))));
        let marked = e(TExprKind::Seq(
            Box::new(f(id("x"))),
            Box::new(f(moved(id("x")))),
        ));
        assert_eq!(strip(&marked), body);
    }

    #[test]
    fn the_last_call_argument_is_moved_and_an_earlier_one_is_not() {
        let f = |x: Typed| call(Callee::User("f".into()), vec![x]);
        let mut body = e(TExprKind::Seq(Box::new(f(id("x"))), Box::new(f(id("x")))));
        let mut alias_outlives = false;
        mark("x", &mut body, true, &HashSet::new(), &mut alias_outlives);
        let TExprKind::Seq(first, last) = &body.kind else {
            panic!("expected a sequence, got {body:?}");
        };
        assert_eq!(**first, f(id("x")));
        assert_eq!(**last, f(moved(id("x"))));
    }
}
