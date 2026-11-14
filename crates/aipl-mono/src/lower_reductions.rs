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

use aipl_syntax::ast::Callee;

/// [`lower_reductions`] on the pinned program: the same rewrite, the nodes it builds typed from
/// what they stand for — the step combines two elements of the sequence being reduced.
pub fn lower_reductions_pinned(
    program: &aipl_syntax::typed::TProgram,
) -> aipl_syntax::typed::TProgram {
    use aipl_syntax::typed::TItem;
    let mut out = program.clone();
    for item in &mut out.items {
        match item {
            TItem::Fn(f) => {
                lower_pinned(&mut f.body);
                if let Some(t) = f.test_body.as_mut() {
                    lower_pinned(t);
                }
            }
            TItem::Struct(s) => {
                for field in &mut s.fields {
                    if let Some(d) = field.default.as_mut() {
                        lower_pinned(d);
                    }
                }
            }
            TItem::Variant(_) => {}
        }
    }
    out
}

fn step_ident(
    step: &Callee,
    sequence: &aipl_syntax::typed::Ty,
    span: &aipl_syntax::Span,
) -> Option<aipl_syntax::typed::TExpr<aipl_syntax::typed::Ty>> {
    use aipl_syntax::typed::{TExpr, TExprKind, Ty};
    let element = match sequence {
        Ty::Array(element) | Ty::Set(element, _) => element.clone(),
        Ty::Primitive(aipl_syntax::ast::Primitive::Str) => {
            Box::new(Ty::Primitive(aipl_syntax::ast::Primitive::Char))
        }
        _ => return None,
    };
    Some(TExpr::new(
        TExprKind::Ident(step.name().to_string()),
        Ty::Fn(vec![(*element).clone(), (*element).clone()], element),
        span.clone(),
    ))
}

fn lower_pinned(e: &mut aipl_syntax::typed::TExpr<aipl_syntax::typed::Ty>) {
    use aipl_syntax::typed::{TExpr, TExprKind, TLambdaParam, Ty};
    crate::typed_util::each_child_mut(e, &mut lower_pinned);
    if let TExprKind::Ident(name) = &e.kind {
        let Some(step) = Callee::from_canonical(name).and_then(|c| c.reduction_step()) else {
            return;
        };
        let Ty::Fn(params, result) = &e.ty else {
            return;
        };
        let [sequence] = params.as_slice() else {
            return;
        };
        let Some(step_ref) = step_ident(&step, sequence, &e.span) else {
            return;
        };
        let param = format!("$reduce{}_xs", aipl_syntax::fresh_id());
        let operand = TExpr::new(
            TExprKind::Ident(param.clone()),
            sequence.clone(),
            e.span.clone(),
        );
        let call = TExpr::new(
            TExprKind::Call {
                callee: Callee::Reduce,
                type_args: Vec::new(),
                args: vec![operand, step_ref],
                method_style: false,
            },
            (**result).clone(),
            e.span.clone(),
        );
        let param = TLambdaParam {
            name: param,
            ty: sequence.clone(),
            span: e.span.clone(),
        };
        e.kind = TExprKind::Lambda(vec![param], Box::new(call));
        return;
    }
    let TExprKind::Call { callee, args, .. } = &mut e.kind else {
        return;
    };
    let (Some(step), [sequence]) = (callee.reduction_step(), args.as_slice()) else {
        return;
    };
    let Some(step_ref) = step_ident(&step, &sequence.ty, &e.span) else {
        return;
    };
    *callee = Callee::Reduce;
    args.push(step_ref);
}
