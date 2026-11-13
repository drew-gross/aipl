//! The ownership questions codegen asks of a finished body, on the typed tree: whether a binding
//! is moved, re-owned, assigned, or kept exclusive, and which parameters an instance only inspects.
//! The rules are the `Expr` ones in `lib.rs` (see the section comment above `inspect_only_params`
//! there), which monomorphization still asks of the bodies it is building.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::Callee;
use aipl_syntax::typed::TExprKind;

use crate::typed_util::{any_child, Typed};
use crate::{builtin_is_mutating, builtin_reads_arg, is_heap_concrete, MonoProgram};

/// Which parameters of each instance in `program` are *borrow-only*: the caller need not retain
/// the argument and the callee must not release it. Keyed by instance name, one flag per parameter
/// (positionally aligned with [`crate::ConcreteFn::params`]). Both codegen call sites read this
/// same map, so the two halves always agree.
pub fn inspect_only_params(program: &MonoProgram) -> HashMap<String, Vec<bool>> {
    // Instances used as function *values* (a bare identifier naming a function, rather than a
    // call of it). Over-approximate — a local binding that happens to share a function's name
    // counts too — which only costs elision.
    let names: HashSet<&str> = program.fns.iter().map(|f| f.name.as_str()).collect();
    let mut address_taken: HashSet<&str> = HashSet::new();
    for f in &program.fns {
        f.body.walk(&mut |e| match &e.kind {
            TExprKind::Ident(n) => {
                if let Some(name) = names.get(n.as_str()) {
                    address_taken.insert(name);
                }
            }
            // A shim installs its functions by address, and the operation's call sites reach
            // them through `call_indirect`, which retains every argument.
            TExprKind::Shim(_, bindings, _) => {
                for (_, shim) in bindings {
                    if let Some(name) = names.get(shim.as_str()) {
                        address_taken.insert(name);
                    }
                }
            }
            _ => {}
        });
    }

    program
        .fns
        .iter()
        .map(|f| {
            // `main` is called by the runtime entry point, not by compiled code: it is handed a
            // freshly built CLI-args array (rc 1) that nothing else owns, so its entry-scope
            // release is what frees it.
            let elidable = f.name != "main"
                && !f.params.iter().any(|p| p.mutable)
                && !address_taken.contains(f.name.as_str());
            let flags = f
                .params
                .iter()
                .map(|p| {
                    elidable
                        && !p.owned
                        && is_heap_concrete(&p.ty)
                        && param_is_inspect_only(&p.name, &f.body)
                })
                .collect();
            (f.name.clone(), flags)
        })
        .collect()
}

/// Whether `body` re-owns the binding `name` through a `mut` binding's slot (`mut y = name` /
/// `set y = name`) — the one shape where codegen may transfer a value's existing reference instead
/// of retaining a fresh one.
pub fn reowns_binding(name: &str, body: &Typed) -> bool {
    !param_is_inspect_only(name, body)
}

/// Whether every use of the parameter `name` in `body` merely inspects it: no use re-owns the
/// incoming reference.
fn param_is_inspect_only(name: &str, body: &Typed) -> bool {
    let is_n = |x: &Typed| matches!(&x.kind, TExprKind::Ident(n) if n == name);
    let mut inspect_only = true;
    body.walk(&mut |e| match &e.kind {
        // `mut y = p` and `set y = p`.
        TExprKind::LetMut(_, val, _) | TExprKind::Assign(_, val, _) if is_n(val) => {
            inspect_only = false;
        }
        _ => {}
    });
    inspect_only
}

/// Whether `body` ever assigns the binding `name` — a `set name = ..`, which is also what every
/// in-place mutation (`set name.push(x)`) desugars to.
pub fn assigns_binding(name: &str, body: &Typed) -> bool {
    if let TExprKind::Assign(lhs, _, _) = &body.kind {
        if matches!(&lhs.kind, TExprKind::Ident(n) if n == name) {
            return true;
        }
    }
    any_child(body, |c| assigns_binding(name, c))
}

/// Whether `body` moves the binding `name` — whole (`__move(name)`) or by a field
/// (`__move(name.f)`), the marks `move_last_use` leaves.
pub fn moves_binding(name: &str, body: &Typed) -> bool {
    if let TExprKind::Call {
        callee: Callee::Move,
        args,
        ..
    } = &body.kind
    {
        let moved = match args.first().map(|a| &a.kind) {
            Some(TExprKind::Ident(n)) => n == name,
            Some(TExprKind::Field(obj, _)) => {
                matches!(&obj.kind, TExprKind::Ident(n) if n == name)
            }
            _ => false,
        };
        if moved {
            return true;
        }
    }
    any_child(body, |c| moves_binding(name, c))
}

/// Whether `name`'s slot can be treated as the sole owner of its block, so a `push`/`extend` into
/// it may write in place instead of copying. `mutating` answers whether a *function* name mutates
/// its receiver.
pub fn binding_is_exclusive(
    name: &str,
    body: &Typed,
    allow_tail_move: bool,
    mutating: &dyn Fn(&str) -> bool,
) -> bool {
    !aliases_or_unsafe(name, body, false, allow_tail_move, mutating)
}

/// Whether `e` is the receiver `name` in a writeback: the bare binding, or the binding handed over
/// with [`Callee::Move`].
pub fn is_receiver(e: &Typed, name: &str) -> bool {
    match &e.kind {
        TExprKind::Ident(n) => n == name,
        TExprKind::Call {
            callee: Callee::Move,
            args,
            ..
        } => {
            matches!(args.as_slice(), [a] if matches!(&a.kind, TExprKind::Ident(n) if n == name))
        }
        _ => false,
    }
}

/// True if `name` is used in `e` in a way that makes in-place mutation unsafe. `iterating` is set
/// inside a `for` over `name`; `tail` means `e` is in tail position, where a use of `name` is a
/// move-out, not an alias. The `Expr` twin in `lib.rs` says why each arm answers as it does.
fn aliases_or_unsafe(
    name: &str,
    e: &Typed,
    iterating: bool,
    tail: bool,
    mutating: &dyn Fn(&str) -> bool,
) -> bool {
    let is_n = |x: &Typed| matches!(&x.kind, TExprKind::Ident(n) if n == name);
    let rec = |x: &Typed| aliases_or_unsafe(name, x, iterating, false, mutating);
    let rec_tail = |x: &Typed| aliases_or_unsafe(name, x, iterating, tail, mutating);
    match &e.kind {
        TExprKind::Shim(_, _, body) => rec_tail(body),
        TExprKind::Ident(n) => n == name && !tail,
        TExprKind::Int(_)
        | TExprKind::Float(_)
        | TExprKind::Bool(_)
        | TExprKind::Str(_)
        | TExprKind::Char(_)
        | TExprKind::None
        | TExprKind::Unit => false,
        TExprKind::Neg(x) | TExprKind::Field(x, _) | TExprKind::Try(x) => is_n(x) || rec(x),
        TExprKind::Return(x) => rec_tail(x),
        TExprKind::Seq(a, b) => rec(a) || rec_tail(b),
        TExprKind::If(c, t, f) => rec(c) || rec_tail(t) || rec_tail(f),
        TExprKind::Index(base, idx) => (!is_n(base) && rec(base)) || rec(idx),
        TExprKind::Slice(obj, start, end) => {
            is_n(obj) || rec(obj) || rec(start) || end.as_ref().is_some_and(|e| rec(e))
        }
        TExprKind::Call {
            callee: fname,
            args,
            method_style,
            ..
        } => {
            if *fname == Callee::MapResult {
                return args.iter().any(|a| if is_n(a) { !tail } else { rec(a) });
            }
            if *fname == Callee::Move {
                return args.iter().any(|a| !is_n(a) && rec(a));
            }
            if aipl_syntax::binop_for_builtin(fname).is_some() || *fname == Callee::LogicalNot {
                return args.iter().any(|a| !is_n(a) && rec(a));
            }
            if *method_style {
                let recv = &args[0];
                let recv_bad = if is_n(recv) {
                    match fname {
                        f if builtin_is_mutating(f.name()) => iterating,
                        Callee::Len
                        | Callee::IsEmpty
                        | Callee::IsNonempty
                        | Callee::ToStr
                        | Callee::ToStrLen
                        | Callee::Trim => false,
                        Callee::With | Callee::WithOrSelf => false,
                        _ => !tail,
                    }
                } else {
                    rec_tail(recv)
                };
                recv_bad
                    || args[1..].iter().enumerate().any(|(k, a)| {
                        if is_n(a) {
                            !tail && !builtin_reads_arg(fname, k + 1)
                        } else {
                            rec_tail(a)
                        }
                    })
            } else {
                let consuming = matches!(
                    fname,
                    Callee::Len
                        | Callee::IsEmpty
                        | Callee::IsNonempty
                        | Callee::ToStr
                        | Callee::ToStrLen
                        | Callee::Print
                        | Callee::Put
                        | Callee::Trim
                        | Callee::FilterKeep
                        | Callee::FilterTruncate
                        | Callee::MapSet
                );
                args.iter().enumerate().any(|(k, a)| {
                    if is_n(a) {
                        let consuming = consuming
                            || (matches!(fname, Callee::With | Callee::WithOrSelf) && k == 0);
                        !consuming && !tail && !builtin_reads_arg(fname, k)
                    } else {
                        rec_tail(a)
                    }
                })
            }
        }
        TExprKind::ArrayLit(elems) | TExprKind::SetLit(elems, _) | TExprKind::TupleLit(elems) => {
            elems
                .iter()
                .any(|x| if is_n(x) { !tail } else { rec_tail(x) })
        }
        TExprKind::DictLit(pairs) => pairs.iter().any(|(k, v)| {
            (if is_n(k) { !tail } else { rec_tail(k) })
                || (if is_n(v) { !tail } else { rec_tail(v) })
        }),
        TExprKind::Construct(_, inits) => inits.iter().any(|i| {
            if is_n(&i.value) {
                !tail
            } else {
                rec_tail(&i.value)
            }
        }),
        TExprKind::Lambda(_, body) => rec(body),
        TExprKind::Let(_, val, b) | TExprKind::LetMut(_, val, b) => {
            is_n(val) || rec(val) || rec_tail(b)
        }
        TExprKind::Assign(lhs, val, b) => {
            let TExprKind::Ident(target) = &lhs.kind else {
                return rec(lhs) || is_n(val) || rec(val) || rec_tail(b);
            };
            let lhs_bad = if target == name {
                match &val.kind {
                    TExprKind::Call {
                        callee: Callee::Trim,
                        args: cargs,
                        ..
                    } if cargs.len() == 1 && is_n(&cargs[0]) => iterating,
                    TExprKind::Call {
                        callee: Callee::Union,
                        args: cargs,
                        ..
                    } if cargs.len() == 2 && is_n(&cargs[0]) => iterating || rec(&cargs[1]),
                    TExprKind::Call {
                        callee: f,
                        args: cargs,
                        ..
                    } if (builtin_is_mutating(f.name()) || mutating(f.name()))
                        && cargs.first().is_some_and(|a| is_receiver(a, name)) =>
                    {
                        iterating || cargs[1..].iter().any(|a| is_n(a) || rec(a))
                    }
                    _ => true,
                }
            } else if is_n(val) {
                !tail
            } else {
                rec(val)
            };
            lhs_bad || rec_tail(b)
        }
        TExprKind::For(_, _, iter, fbody) => {
            if is_n(iter) {
                aliases_or_unsafe(name, fbody, true, false, mutating)
            } else {
                rec(iter) || aliases_or_unsafe(name, fbody, iterating, false, mutating)
            }
        }
        TExprKind::While(cond, wbody) => rec(cond) || rec(wbody),
        TExprKind::Match(scrut, arms) => {
            is_n(scrut) || rec(scrut) || arms.iter().any(|arm| rec_tail(&arm.body))
        }
        TExprKind::IfLet(arm, scrut, else_b) => {
            is_n(scrut) || rec(scrut) || rec_tail(&arm.body) || rec_tail(else_b)
        }
    }
}
