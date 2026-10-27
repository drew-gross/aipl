//! Lower every reduction to the one implementation of reducing.
//!
//! A reduction is a builtin that reduces a sequence to one value by folding it
//! with a binary step, and which step is the whole of what distinguishes one from
//! another ([`Callee::reduction_step`]). So a reduction needs no implementation of
//! its own: `xs.minimum()` *is* `xs.reduce(min)`, where `reduce` is the single
//! generic fold and `min` is the code that makes it a minimum. This pass writes
//! that down, replacing the call and passing the step as an argument:
//!
//! ```text
//! xs.minimum()   →   xs.reduce(min)
//! xs.maximum()   →   xs.reduce(max)
//! ```
//!
//! Adding a reduction is therefore a line in [`Callee::reduction_step`] and
//! nothing else: no codegen arm, no AIPL source, no fused call of its own.
//!
//! # Where it runs
//!
//! Between the optimizer and monomorphization, and not as one of the optimizer's
//! passes. Fusion reads the *reduction* — `xs.map(f).minimum()` is a chain whose
//! outer call is one — and turns it into `map_reduce(xs, f, min)`; a `reduce`
//! written here first would hide the pair from it. And it cannot be optional:
//! nothing below this point knows how to compile a reduction call, so a program
//! that skipped it would fail in codegen.
//!
//! A reduction named as a *value* — `measure(xs, minimum)`, `arrays.map(minimum)` —
//! is the same thing without its argument, and becomes the function that supplies
//! it: `|xs| xs.reduce(min)`, the shape `lower_ctor_refs` gives a constructor used
//! as a function.
//!
//! Every function body and test body is rewritten, and the struct field defaults,
//! which are expressions too. The step is named as a call to it resolves from, the
//! same spelling the fusion pass uses for the step it passes.

use aipl_syntax::ast::{Callee, Expr, ExprKind, FieldDecl, Item, LambdaParam, Program};

/// `program` with every reduction call written as a call to `reduce`.
pub fn lower_reductions(program: &Program) -> Program {
    Program {
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) => {
                    let mut f = f.clone();
                    lower_expr(&mut f.body);
                    if let Some(t) = f.test_body.as_mut() {
                        lower_expr(t);
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
                                lower_expr(d);
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
fn lower_expr(e: &mut Expr) {
    for child in crate::children_mut(e) {
        lower_expr(child);
    }
    // The name of a reduction in value position.
    if let ExprKind::Ident(name) = &e.kind {
        let Some(step) = Callee::from_canonical(name).and_then(|c| c.reduction_step()) else {
            return;
        };
        let param = format!("$reduce{}_xs", aipl_syntax::fresh_id());
        let operand = Expr::new(ExprKind::Ident(param.clone()), e.span.clone());
        let call = Expr::new(
            ExprKind::Call(
                Callee::Reduce,
                vec![
                    operand,
                    Expr::new(ExprKind::Ident(step.name().to_string()), e.span.clone()),
                ],
                false,
            ),
            e.span.clone(),
        );
        let param = LambdaParam {
            name: param,
            ty: None,
            span: e.span.clone(),
        };
        e.kind = ExprKind::Lambda(vec![param], Box::new(call));
        return;
    }
    let ExprKind::Call(callee, args, _) = &mut e.kind else {
        return;
    };
    // One operand: the sequence. A call with any other count is not a reduction
    // as the checker has typed it, and is left for codegen to report.
    let (Some(step), [_]) = (callee.reduction_step(), args.as_slice()) else {
        return;
    };
    *callee = Callee::Reduce;
    args.push(Expr::new(
        ExprKind::Ident(step.name().to_string()),
        e.span.clone(),
    ));
}
