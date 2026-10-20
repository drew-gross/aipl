//! `xs.with(i, v).value_or(xs)` as one operation that never needs `xs` twice.
//!
//! `with` answers `none` for an index past the end, and the natural way to keep
//! the array in that case is to fall back to the receiver. Written out that reads
//! the receiver twice — once to give to `with`, once as the default — and the
//! first of those is the one that wants to be a *move*: `with` writes into an
//! array nobody else holds. A receiver that is read again afterwards is held by
//! the second read, so the write copies.
//!
//! ```text
//! xs.with(i, v).value_or(xs)   →   __with_or_self(xs, i, v)
//! ```
//!
//! The rewritten call consumes `xs` once and answers the updated array, or `xs`
//! itself when `i` is past the end — the same value, with nothing left over to
//! read. It also means a function whose only other use of a parameter is that
//! default is not disqualified from owning it (`fields_read_once`).
//!
//! The receiver and the default must be the same *place*: a binding, or a field
//! path off one. Nothing else is known to be the same value twice.
//!
//! Where it runs: with `lower_reductions`, after the optimizer and before
//! monomorphization, over every function body, test body and field default.

use aipl_syntax::ast::{Callee, Expr, ExprKind, FieldDecl, Item, Program};

/// `program` with each `x.with(i, v).value_or(x)` written as `__with_or_self(x, i, v)`.
pub fn fuse_with_or_self(program: &Program) -> Program {
    Program {
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) => {
                    let mut f = f.clone();
                    fuse_expr(&mut f.body);
                    if let Some(t) = f.test_body.as_mut() {
                        fuse_expr(t);
                    }
                    Item::Fn(f)
                }
                Item::Struct(s) => {
                    let mut s = s.clone();
                    s.fields = s
                        .fields
                        .iter()
                        .map(|fd| {
                            let mut default = fd.default.clone();
                            if let Some(d) = default.as_mut() {
                                fuse_expr(d);
                            }
                            FieldDecl {
                                name: fd.name.clone(),
                                ty: fd.ty.clone(),
                                default,
                            }
                        })
                        .collect();
                    Item::Struct(s)
                }
                other => other.clone(),
            })
            .collect(),
    }
}

/// Rewrite `e` in place, children first.
fn fuse_expr(e: &mut Expr) {
    for child in crate::children_mut(e) {
        fuse_expr(child);
    }
    let ExprKind::Call(Callee::ValueOr, args, style) = &mut e.kind else {
        return;
    };
    let [with, default] = args.as_mut_slice() else {
        return;
    };
    let ExprKind::Call(Callee::With, with_args, _) = &with.kind else {
        return;
    };
    let [receiver, _, _] = with_args.as_slice() else {
        return;
    };
    if !same_place(receiver, default) {
        return;
    }
    let ExprKind::Call(_, with_args, _) = std::mem::replace(&mut with.kind, ExprKind::Unit) else {
        unreachable!("matched a call to `with` just above")
    };
    e.kind = ExprKind::Call(Callee::WithOrSelf, with_args, *style);
}

/// Whether `a` and `b` read the same binding, or the same field path off one.
fn same_place(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Ident(x), ExprKind::Ident(y)) => x == y,
        (ExprKind::Field(x, f), ExprKind::Field(y, g)) => f == g && same_place(x, y),
        _ => false,
    }
}
