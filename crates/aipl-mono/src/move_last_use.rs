//! Moving a binding into the call that ends its life.
//!
//! A heap value handed to a user function is *borrowed* by default: the caller
//! retains it before the call and the callee releases it on return. When the
//! argument is a fresh temporary — a call result, a literal — codegen already
//! *moves* it instead, transferring the caller's sole reference and skipping
//! both halves of that pair. A named binding at its last use is the same
//! situation with a name on it: after the call nothing reads the binding, so
//! its reference can go with the value.
//!
//! This pass finds those uses and marks them, by wrapping the argument in the
//! `__move` intrinsic ([`Callee::Move`]):
//!
//! ```text
//! let xs = build();               let xs = build();
//! print(xs.len());          →     print(xs.len());
//! consume(xs)                     consume(__move(xs))
//! ```
//!
//! Everything downstream then treats `__move(xs)` as it treats a fresh call
//! result — mono's `owned_for_call` picks the callee's owned instance for it,
//! and codegen's argument hand-off moves it — while codegen's lowering of the
//! intrinsic itself is what makes that true: it turns the binding's own
//! reference into a tracked temporary (see the `Callee::Move` arm in
//! `aipl-codegen`).
//!
//! # What is a last use
//!
//! An argument occurrence after which the binding is never read on any path
//! that continues in this function. The walk carries one bit, *tail*: whether
//! the binding is dead once the current expression finishes. It starts true
//! at the binding's whole scope and is narrowed by anything that runs
//! afterwards and mentions the name — the rest of a sequence, the body of a
//! `let`, the later arguments of a call, the branches after a condition. A
//! `return` resets it to true (nothing runs after one), and a loop body clears
//! it (the next iteration does). A `set x = f(x)` is the one place the old
//! value dies unconditionally — the assignment replaces it — so the right-hand
//! side is analysed with tail set whatever follows.
//!
//! # Why the binding has to be *all* call arguments
//!
//! A move hands the callee the reference the binding held. That is only sound
//! if nothing else was relying on that reference without holding its own — and
//! codegen's borrows are exactly that: `let a = x`, `x.f`, `x[i]` and `x[..]`
//! all read through `x`'s reference rather than retaining one. A call result is
//! refcounted, so an occurrence *as a call argument* never creates such a
//! borrow; any other occurrence might. So a binding qualifies only when every
//! one of its occurrences is a direct argument of some call — user or builtin,
//! it does not matter which for this question — and none sits inside a lambda,
//! where it would be a capture. Rebinding the name inside its own scope is
//! declined too, rather than reasoned about.
//!
//! The receiver of a mutating method (`fn f(mut self: ..)`) is left alone: the
//! `set x = x.f(..)` writeback is recognised by mono on the bare name, and a
//! wrapped receiver would fall out of that path into the copying one.
//!
//! Only `Callee::User` calls take a mark: a builtin's per-argument contract is
//! its own (codegen never moves into one), and the mark would only cost the
//! shape a builtin's in-place lowering matches on.
//!
//! A binding whose value is another binding (`let a = x`) never qualifies: the
//! two name one reference, and codegen finds a reference by its value, so a
//! move of either would take what the other still reads through. Seen from
//! `x`'s side that same `let a = x` is either its last use — then it is a
//! move, `let a = __move(x)`, and `a` goes on to own the value (this is what
//! every inlined call becomes: `f(__move(x))` is `let p = x; ..` afterwards)
//! — or it is an alias that outlives the point where `x` is used again, in
//! which case *nothing* of `x` may move: a later move would take the
//! reference `a` still reads through. So an alias-let that is not last
//! withdraws every mark on the binding, not just its own.
//!
//! A struct binding is read through its fields, and a field read is a borrow
//! of that field. So for a struct the rule is refined: a read of a field that
//! owns no heap (`acc.nullable`) is a plain copy and does not count, while a
//! read of one that does (`acc.kinds`) disqualifies like any other borrow.
//! Which fields those are comes from the caller (`heap_fields`), who has the
//! struct's declaration.
//!
//! # When it runs — twice
//!
//! First from mono's `infer`, at each `let`/`mut` whose value is heap-typed:
//! the one place that knows both the binding's type and its whole scope, and
//! that comes before `owned_for_call` chooses each callee's instance — a
//! marked argument is what earns the callee's owned instance, with its
//! in-place body.
//!
//! Then again as the last thing before codegen, over the final bodies
//! ([`move_last_uses_post_mono`]): every mark stripped and the analysis
//! re-run. The passes in between — inlining, sinking, the constructor
//! eliminator — move *pure* expressions to where their value is used, and a
//! read of a binding is pure to them; but a read moved past the binding's
//! `__move` reads a value that has been taken. A mark is only as good as the
//! evaluation order it was computed against, so the order that codegen will
//! actually follow is the one that decides. A mark the early run made that no
//! longer holds is simply not remade: the argument is then a borrow, retained
//! like any other, and an owned instance that receives one consumes that
//! retained reference — the accounting balances either way.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, ConcreteType, Expr, ExprKind, MatchArm, Type};

use crate::{children, children_mut, count_ident, is_heap_concrete, ConcreteFn, MonoProgram};

/// `body` — the scope of the binding `name`, whose initializer is `value` —
/// with each last use of `name` that is an argument to a user function wrapped
/// in `__move`. Unchanged when the binding does not qualify (see the module
/// docs). `mutating` names the functions with a `mut self` receiver.
pub(crate) fn move_last_uses(
    name: &str,
    value: &Expr,
    body: &Expr,
    heap_fields: &HashSet<String>,
    mutating: &HashSet<String>,
) -> Expr {
    if matches!(&value.kind, ExprKind::Ident(_))
        || !only_call_arguments(name, body, false, heap_fields)
    {
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

/// `body` — the scope of the struct binding `name`, whose initializer is
/// `value` — with each read of one of its `heap_fields` wrapped in `__move`,
/// when the binding owns its value and reads each such field at most once
/// and never in a loop (`fields_read_once`, which also rules out any use of
/// the value as a whole). Those reads are then *moves* of the fields: codegen
/// takes each field's reference out of the struct instead of retaining a
/// copy, and the struct's own release passes over what was taken. The
/// field-by-field counterpart of a whole-value move, and disjoint from it — a
/// binding used whole is never field-moved, and one with a heap field read is
/// never moved whole.
///
/// Each read is its own last use by construction, so no tail analysis is
/// needed, and a later pass may move the read wherever it moves the value:
/// nothing else reads that field.
pub(crate) fn move_field_reads(
    name: &str,
    value: &Expr,
    body: &Expr,
    heap_fields: &HashSet<String>,
) -> Expr {
    if heap_fields.is_empty()
        || matches!(&value.kind, ExprKind::Ident(_))
        || !crate::fields_read_once(name, body)
    {
        return body.clone();
    }
    let mut out = body.clone();
    mark_fields(name, &mut out, heap_fields);
    out
}

fn mark_fields(name: &str, e: &mut Expr, heap_fields: &HashSet<String>) {
    let take =
        matches!(&e.kind, ExprKind::Field(obj, f) if is_name(obj, name) && heap_fields.contains(f));
    if take {
        let span = e.span.clone();
        let inner = std::mem::replace(e, Expr::new(ExprKind::Unit, span.clone()));
        *e = Expr::new(ExprKind::Call(Callee::Move, vec![inner], false), span);
        return;
    }
    for child in children_mut(e) {
        mark_fields(name, child, heap_fields);
    }
}

/// The final marking, over the whole program: every `__move` the early run
/// left is stripped and the analysis re-run on each binding, so the marks
/// codegen sees are the ones that hold in the order it will evaluate. See the
/// module docs for why the early marks cannot be kept.
pub fn move_last_uses_post_mono(program: &MonoProgram) -> MonoProgram {
    let mutating: HashSet<String> = program
        .fns
        .iter()
        .filter(|f| f.is_mutating())
        .map(|f| f.name.clone())
        .collect();
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| ConcreteFn {
                body: remark(&strip(&f.body), program, &mutating),
                ..f.clone()
            })
            .collect(),
        ..program.clone()
    }
}

/// The heap-owning fields of the struct `ty` names, from the finished
/// program's declarations — empty for anything but a struct. The post-mono
/// twin of `Mono::heap_fields_of`; a `let` of a struct carries its type by
/// then (`infer` records it), which is what makes the final marking able to
/// tell a scalar field read from a borrow.
fn heap_fields_post_mono(program: &MonoProgram, ty: &Type) -> HashSet<String> {
    let Type::Named(name) = ty else {
        return HashSet::new();
    };
    program
        .structs
        .iter()
        .find(|d| &d.name == name)
        .map(|d| {
            d.fields
                .iter()
                .filter(|f| owns_heap_post_mono(program, &f.ty))
                .map(|f| f.name.clone())
                .collect()
        })
        .unwrap_or_default()
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

/// `e` with every `__move(x)` back to `x`.
fn strip(e: &Expr) -> Expr {
    if let ExprKind::Call(Callee::Move, args, _) = &e.kind {
        if let [inner] = args.as_slice() {
            return strip(inner);
        }
    }
    let mut out = e.clone();
    for child in children_mut(&mut out) {
        *child = strip(child);
    }
    out
}

/// `e` with every binding under it marked, innermost first — a binding's marks
/// touch only its own name, so the order does not matter for correctness and
/// inner-first means each outer analysis sees its scope in final form.
fn remark(e: &Expr, program: &MonoProgram, mutating: &HashSet<String>) -> Expr {
    let mut out = e.clone();
    for child in children_mut(&mut out) {
        *child = remark(child, program, mutating);
    }
    match &out.kind {
        ExprKind::Let(name, ty, value, body) | ExprKind::LetMut(name, ty, value, body) => {
            let heap_fields = ty
                .as_ref()
                .map(|t| heap_fields_post_mono(program, t))
                .unwrap_or_default();
            let body = move_last_uses(name, value, body, &heap_fields, mutating);
            let body = move_field_reads(name, value, &body, &heap_fields);
            match &out.kind {
                ExprKind::Let(n, t, v, _) => Expr {
                    kind: ExprKind::Let(n.clone(), t.clone(), v.clone(), Box::new(body)),
                    ..out.clone()
                },
                ExprKind::LetMut(n, t, v, _) => Expr {
                    kind: ExprKind::LetMut(n.clone(), t.clone(), v.clone(), Box::new(body)),
                    ..out.clone()
                },
                _ => unreachable!(),
            }
        }
        _ => out,
    }
}

fn mentions(name: &str, e: &Expr) -> bool {
    count_ident(name, e) > 0
}

fn is_name(e: &Expr, name: &str) -> bool {
    matches!(&e.kind, ExprKind::Ident(n) if n == name)
}

fn binds(arm: &MatchArm, name: &str) -> bool {
    arm.pattern.bindings().iter().any(|b| b == name)
}

/// Whether every occurrence of `name` in `e` is a direct call argument — or,
/// for a struct, a read of one of its fields outside `heap_fields` — with none
/// under a lambda and no rebinding of the name. `as_argument` says whether `e`
/// itself sits in argument position.
fn only_call_arguments(
    name: &str,
    e: &Expr,
    as_argument: bool,
    heap_fields: &HashSet<String>,
) -> bool {
    let all = |es: &[&Expr]| {
        es.iter()
            .all(|c| only_call_arguments(name, c, false, heap_fields))
    };
    match &e.kind {
        ExprKind::Ident(n) => n != name || as_argument,
        // A copy of a scalar field is not a borrow of the value.
        ExprKind::Field(obj, f) if is_name(obj, name) => !heap_fields.contains(f),
        // `let a = name` is a move or an alias; `mark` decides which, and an
        // alias withdraws everything. Either way it is not a borrow that
        // disqualifies here.
        ExprKind::Let(n, _, value, body) | ExprKind::LetMut(n, _, value, body)
            if n != name && is_name(value, name) =>
        {
            only_call_arguments(name, body, false, heap_fields)
        }
        ExprKind::Call(_, args, _) => args
            .iter()
            .all(|a| only_call_arguments(name, a, true, heap_fields)),
        ExprKind::Lambda(params, body) => {
            params.iter().all(|p| p.name != name) && !mentions(name, body)
        }
        ExprKind::Let(n, _, _, _) | ExprKind::LetMut(n, _, _, _) | ExprKind::For(n, _, _)
            if n == name =>
        {
            false
        }
        // The target of `set name = ..` is a place, not a read of the value;
        // a field path (`set name.f = ..`) reads it, and is checked as one.
        ExprKind::Assign(lhs, value, rest) => {
            (is_name(lhs, name) || only_call_arguments(name, lhs, false, heap_fields))
                && all(&[value, rest])
        }
        ExprKind::Match(scrutinee, arms) => {
            arms.iter().all(|a| !binds(a, name))
                && only_call_arguments(name, scrutinee, false, heap_fields)
                && arms
                    .iter()
                    .all(|a| only_call_arguments(name, &a.body, false, heap_fields))
        }
        ExprKind::IfLet(arm, scrutinee, else_body) => {
            !binds(arm, name) && all(&[scrutinee, &arm.body, else_body])
        }
        _ => all(&children(e)),
    }
}

/// Wrap each last-use argument of `name` under `e`. `tail` is whether the
/// binding is dead once `e` finishes. `alias_outlives` is set when a
/// `let a = name` is found that is not `name`'s last use — see the module
/// docs for why that withdraws every mark.
fn mark(
    name: &str,
    e: &mut Expr,
    tail: bool,
    mutating: &HashSet<String>,
    alias_outlives: &mut bool,
) {
    match &mut e.kind {
        ExprKind::Let(_, _, value, body) | ExprKind::LetMut(_, _, value, body)
            if is_name(value, name) =>
        {
            if tail && !mentions(name, body) {
                let span = value.span.clone();
                let inner =
                    std::mem::replace(value.as_mut(), Expr::new(ExprKind::Unit, span.clone()));
                *value = Box::new(Expr::new(
                    ExprKind::Call(Callee::Move, vec![inner], false),
                    span,
                ));
            } else {
                *alias_outlives = true;
            }
            mark(name, body, tail, mutating, alias_outlives);
        }
        ExprKind::Call(callee, args, _) => {
            let user = matches!(callee, Callee::User(f) if !mutating.contains(f));
            // Arguments are evaluated left to right, and each is handed over
            // only once all of them are — but a later argument that reads the
            // binding is a later use all the same.
            let later: Vec<bool> = (0..args.len())
                .map(|i| args[i + 1..].iter().any(|a| mentions(name, a)))
                .collect();
            for (arg, later) in args.iter_mut().zip(later) {
                let t = tail && !later;
                if user && t && is_name(arg, name) {
                    let span = arg.span.clone();
                    let inner = std::mem::replace(arg, Expr::new(ExprKind::Unit, span.clone()));
                    *arg = Expr::new(ExprKind::Call(Callee::Move, vec![inner], false), span);
                } else {
                    mark(name, arg, t, mutating, alias_outlives);
                }
            }
        }
        ExprKind::Seq(first, rest) => {
            let t = tail && !mentions(name, rest);
            mark(name, first, t, mutating, alias_outlives);
            mark(name, rest, tail, mutating, alias_outlives);
        }
        ExprKind::Let(_, _, value, body) | ExprKind::LetMut(_, _, value, body) => {
            let t = tail && !mentions(name, body);
            mark(name, value, t, mutating, alias_outlives);
            mark(name, body, tail, mutating, alias_outlives);
        }
        ExprKind::Assign(lhs, value, rest) => {
            // Reassigning the binding itself ends its old value here, whatever
            // reads the new one afterwards.
            let t = if is_name(lhs, name) {
                true
            } else {
                tail && !mentions(name, rest)
            };
            mark(name, value, t, mutating, alias_outlives);
            mark(name, rest, tail, mutating, alias_outlives);
        }
        ExprKind::If(cond, then_body, else_body) => {
            let t = tail && !mentions(name, then_body) && !mentions(name, else_body);
            mark(name, cond, t, mutating, alias_outlives);
            mark(name, then_body, tail, mutating, alias_outlives);
            mark(name, else_body, tail, mutating, alias_outlives);
        }
        ExprKind::IfLet(arm, scrutinee, else_body) => {
            let t = tail && !mentions(name, &arm.body) && !mentions(name, else_body);
            mark(name, scrutinee, t, mutating, alias_outlives);
            mark(name, &mut arm.body, tail, mutating, alias_outlives);
            mark(name, else_body, tail, mutating, alias_outlives);
        }
        ExprKind::Match(scrutinee, arms) => {
            let t = tail && arms.iter().all(|a| !mentions(name, &a.body));
            mark(name, scrutinee, t, mutating, alias_outlives);
            for arm in arms {
                mark(name, &mut arm.body, tail, mutating, alias_outlives);
            }
        }
        // A use inside a loop is followed by the next iteration.
        ExprKind::For(_, iter, body) => {
            let t = tail && !mentions(name, body);
            mark(name, iter, t, mutating, alias_outlives);
            mark(name, body, false, mutating, alias_outlives);
        }
        ExprKind::While(cond, body) => {
            mark(name, cond, false, mutating, alias_outlives);
            mark(name, body, false, mutating, alias_outlives);
        }
        // Nothing runs after a `return`.
        ExprKind::Return(value) => mark(name, value, true, mutating, alias_outlives),
        // A capture, never a move; `only_call_arguments` has already ruled the
        // name out of here.
        ExprKind::Lambda(..) => {}
        // Every other node evaluates its children in order and keeps none of
        // them alive past itself.
        _ => {
            let later: Vec<bool> = {
                let kids = children(e);
                (0..kids.len())
                    .map(|i| kids[i + 1..].iter().any(|c| mentions(name, c)))
                    .collect()
            };
            for (child, later) in children_mut(e).into_iter().zip(later) {
                mark(name, child, tail && !later, mutating, alias_outlives);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: ExprKind) -> Expr {
        Expr::new(kind, 0..0)
    }
    fn id(n: &str) -> Expr {
        e(ExprKind::Ident(n.to_string()))
    }
    fn call(f: &str, args: Vec<Expr>) -> Expr {
        e(ExprKind::Call(Callee::User(f.to_string()), args, false))
    }
    fn seq(a: Expr, b: Expr) -> Expr {
        e(ExprKind::Seq(Box::new(a), Box::new(b)))
    }
    fn moved(n: &str) -> Expr {
        e(ExprKind::Call(Callee::Move, vec![id(n)], false))
    }
    fn run(body: Expr) -> Expr {
        move_last_uses(
            "x",
            &call("mk", vec![]),
            &body,
            &HashSet::new(),
            &HashSet::new(),
        )
    }

    #[test]
    fn last_argument_use_moves() {
        let body = seq(call("f", vec![id("x")]), call("g", vec![id("x")]));
        let want = seq(call("f", vec![id("x")]), call("g", vec![moved("x")]));
        assert_eq!(run(body), want);
    }

    #[test]
    fn a_later_read_keeps_the_borrow() {
        let body = seq(
            call("f", vec![id("x")]),
            e(ExprKind::Call(Callee::Len, vec![id("x")], true)),
        );
        assert_eq!(run(body.clone()), body);
    }

    #[test]
    fn builtins_are_never_marked() {
        let body = e(ExprKind::Call(Callee::Len, vec![id("x")], true));
        assert_eq!(run(body.clone()), body);
    }

    #[test]
    fn a_bare_read_disqualifies_the_binding() {
        // `let a = x; g(x)` — `a` borrows through `x`'s reference.
        let body = e(ExprKind::Let(
            "a".to_string(),
            None,
            Box::new(id("x")),
            Box::new(call("g", vec![id("x")])),
        ));
        assert_eq!(run(body.clone()), body);
        // `x.f` likewise, when `f` owns heap (the scalar case is tested below).
        let body = seq(
            e(ExprKind::Field(Box::new(id("x")), "f".to_string())),
            call("g", vec![id("x")]),
        );
        let heap: HashSet<String> = ["f".to_string()].into_iter().collect();
        assert_eq!(
            move_last_uses("x", &call("mk", vec![]), &body, &heap, &HashSet::new()),
            body
        );
    }

    #[test]
    fn each_branch_has_its_own_last_use() {
        let body = e(ExprKind::If(
            Box::new(id("c")),
            Box::new(call("f", vec![id("x")])),
            Box::new(call("g", vec![id("x")])),
        ));
        let want = e(ExprKind::If(
            Box::new(id("c")),
            Box::new(call("f", vec![moved("x")])),
            Box::new(call("g", vec![moved("x")])),
        ));
        assert_eq!(run(body), want);
    }

    #[test]
    fn a_use_in_a_loop_is_not_last() {
        let body = e(ExprKind::While(
            Box::new(id("c")),
            Box::new(call("f", vec![id("x")])),
        ));
        assert_eq!(run(body.clone()), body);
    }

    #[test]
    fn reassignment_ends_the_old_value() {
        // `set x = f(x); g(x)` — the `f` argument is the old value's last use.
        let body = e(ExprKind::Assign(
            Box::new(id("x")),
            Box::new(call("f", vec![id("x")])),
            Box::new(call("g", vec![id("x")])),
        ));
        let want = e(ExprKind::Assign(
            Box::new(id("x")),
            Box::new(call("f", vec![moved("x")])),
            Box::new(call("g", vec![moved("x")])),
        ));
        assert_eq!(run(body), want);
    }

    #[test]
    fn a_later_argument_is_a_later_use() {
        let body = call(
            "f",
            vec![id("x"), e(ExprKind::Call(Callee::Len, vec![id("x")], true))],
        );
        assert_eq!(run(body.clone()), body);
    }

    #[test]
    fn a_return_is_final() {
        let body = seq(
            e(ExprKind::Return(Box::new(call("f", vec![id("x")])))),
            call("g", vec![id("x")]),
        );
        let want = seq(
            e(ExprKind::Return(Box::new(call("f", vec![moved("x")])))),
            call("g", vec![moved("x")]),
        );
        assert_eq!(run(body), want);
    }

    #[test]
    fn an_alias_never_moves() {
        // `let x = y; f(x)` — `x` and `y` name one reference.
        let body = call("f", vec![id("x")]);
        assert_eq!(
            move_last_uses("x", &id("y"), &body, &HashSet::new(), &HashSet::new()),
            body
        );
    }

    #[test]
    fn stripping_undoes_marking() {
        let body = seq(call("f", vec![id("x")]), call("g", vec![id("x")]));
        assert_eq!(strip(&run(body.clone())), body);
    }

    #[test]
    fn a_scalar_field_read_is_not_a_borrow() {
        // `x.n` copies a scalar; `x.kinds` borrows a set.
        let read = |f: &str| e(ExprKind::Field(Box::new(id("x")), f.to_string()));
        let heap: HashSet<String> = ["kinds".to_string()].into_iter().collect();
        let body = seq(read("n"), call("f", vec![id("x")]));
        let want = seq(read("n"), call("f", vec![moved("x")]));
        assert_eq!(
            move_last_uses("x", &call("mk", vec![]), &body, &heap, &HashSet::new()),
            want
        );
        let body = seq(read("kinds"), call("f", vec![id("x")]));
        assert_eq!(
            move_last_uses("x", &call("mk", vec![]), &body, &heap, &HashSet::new()),
            body
        );
    }

    #[test]
    fn heap_field_reads_of_an_owned_struct_move() {
        let read = |f: &str| e(ExprKind::Field(Box::new(id("x")), f.to_string()));
        let heap: HashSet<String> = ["kinds".to_string()].into_iter().collect();
        let body = seq(read("kinds"), read("n"));
        let want = seq(
            e(ExprKind::Call(Callee::Move, vec![read("kinds")], false)),
            read("n"),
        );
        assert_eq!(
            move_field_reads("x", &call("mk", vec![]), &body, &heap),
            want
        );
        // Read twice, or used whole, or an alias: left alone.
        let twice = seq(read("kinds"), read("kinds"));
        assert_eq!(
            move_field_reads("x", &call("mk", vec![]), &twice, &heap),
            twice
        );
        let whole = seq(read("kinds"), call("f", vec![id("x")]));
        assert_eq!(
            move_field_reads("x", &call("mk", vec![]), &whole, &heap),
            whole
        );
        assert_eq!(move_field_reads("x", &id("y"), &body, &heap), body);
    }

    #[test]
    fn a_last_use_rebinding_moves() {
        // `let y = x; g(y)` — what an inlined `g(x)` becomes.
        let body = e(ExprKind::Let(
            "y".to_string(),
            None,
            Box::new(id("x")),
            Box::new(call("g", vec![id("y")])),
        ));
        let want = e(ExprKind::Let(
            "y".to_string(),
            None,
            Box::new(moved("x")),
            Box::new(call("g", vec![id("y")])),
        ));
        assert_eq!(run(body), want);
    }

    #[test]
    fn an_alias_that_outlives_withdraws_every_mark() {
        // `let y = x; f(x)` — `y` still reads through `x`'s reference, so
        // even the last use of `x` must not move.
        let body = e(ExprKind::Let(
            "y".to_string(),
            None,
            Box::new(id("x")),
            Box::new(call("f", vec![id("x")])),
        ));
        assert_eq!(run(body.clone()), body);
        // In the other order `f(x)` is not last, and the rebinding is.
        let body = seq(
            call("f", vec![id("x")]),
            e(ExprKind::Let(
                "y".to_string(),
                None,
                Box::new(id("x")),
                Box::new(id("y")),
            )),
        );
        let want = seq(
            call("f", vec![id("x")]),
            e(ExprKind::Let(
                "y".to_string(),
                None,
                Box::new(moved("x")),
                Box::new(id("y")),
            )),
        );
        assert_eq!(run(body), want);
    }

    #[test]
    fn a_mutating_receiver_is_left_alone() {
        let body = e(ExprKind::Call(
            Callee::User("f".to_string()),
            vec![id("x")],
            true,
        ));
        let mutating: HashSet<String> = ["f".to_string()].into_iter().collect();
        assert_eq!(
            move_last_uses("x", &call("mk", vec![]), &body, &HashSet::new(), &mutating),
            body
        );
    }

    #[test]
    fn a_capture_disqualifies_the_binding() {
        let lambda = e(ExprKind::Lambda(vec![], Box::new(call("g", vec![id("x")]))));
        let body = seq(
            e(ExprKind::Call(Callee::Map, vec![id("ys"), lambda], true)),
            call("f", vec![id("x")]),
        );
        assert_eq!(run(body.clone()), body);
    }
}
