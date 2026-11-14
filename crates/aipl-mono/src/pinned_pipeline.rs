//! What carries a pinned program through the pre-monomorphization pipeline and hands it on:
//! the rewrite that gives each `int_parse` the call its target type asks for, and writing the
//! optimized pinned functions back into the program monomorphization reads.

use aipl_syntax::ast::{Callee, Item, Primitive, Program};
use aipl_syntax::typed::{TExpr, TExprKind, TItem, TProgram, Ty};

use crate::pin::{Instances, Pinned};
use crate::probe::{stamp_everything, Stamping};
use crate::typed_util::{each_child_mut, map_pinned_bodies};

/// What the optimizer starts from: the pinned program with the checker's view of refinements
/// erased, and with each
/// An `int_parse` typed as producing an integer other than `i64`, as the calls that produce it: the
/// signed parse for a signed target, the unsigned one for an unsigned target (so a `u64` reaches
/// past `i64::MAX`), each narrowed to the target unless it already is one.
pub fn retarget_int_parses_pinned(program: &TProgram) -> TProgram {
    map_pinned_bodies(program, |_| true, |_, body| retarget(body))
}

fn retarget(e: &mut TExpr<Ty>) {
    each_child_mut(e, &mut retarget);
    // The checker's view of a refined array is the array it is at run time.
    if let TExprKind::Call {
        callee: Callee::AssumeNonempty,
        args,
        ..
    } = &mut e.kind
    {
        if args.len() == 1 {
            *e = args.pop().expect("one argument");
            return;
        }
    }
    let TExprKind::Call {
        callee: Callee::IntParse,
        type_args,
        args,
        method_style,
    } = &e.kind
    else {
        return;
    };
    let Ty::Optional(core) = &e.ty else {
        return;
    };
    let Ty::Primitive(target) = **core else {
        return;
    };
    let (parse, wide) = if target.int_signed() {
        (Callee::IntParse, Primitive::I64)
    } else {
        (Callee::IntParseUnsigned, Primitive::U64)
    };
    let wide_ty = Ty::Optional(Box::new(Ty::Primitive(wide)));
    let call = |ty: Ty| {
        TExpr::rebuilt(
            TExprKind::Call {
                callee: parse.clone(),
                type_args: type_args.clone(),
                args: args.clone(),
                method_style: *method_style,
            },
            ty,
            e,
        )
    };
    if matches!(target, Primitive::I64 | Primitive::U64) {
        let ty = e.ty.clone();
        *e = call(ty);
        return;
    }
    let inner = call(wide_ty);
    *e = TExpr::rebuilt(
        TExprKind::Call {
            callee: Callee::Narrow,
            type_args: Vec::new(),
            args: vec![inner],
            method_style: true,
        },
        e.ty.clone(),
        e,
    );
}

/// The program monomorphization reads, from the pinned one alone: its functions, structs and
/// variants, with every expression stamped with the type `typed` gives it.
pub fn to_program(typed: &TProgram, instances: &Instances) -> Program {
    let mut program = typed.erase();
    for item in &mut program.items {
        if let Item::Fn(f) = item {
            anonymous_any_again(&mut f.sig);
        }
    }
    let pinned = Pinned {
        program: typed.clone(),
        failures: Vec::new(),
        declared: 0,
        instances: instances.clone(),
        resolutions: Vec::new(),
    };
    stamp_everything(&mut program, &pinned, Stamping::Typed);
    stamp_test_bodies(&mut program, typed, instances);
    program
}

/// A signature as it was written: the pinner makes each bare `any` a type variable of its own, and
/// here each is a bare `any` again.
fn anonymous_any_again(sig: &mut aipl_syntax::ast::Signature) {
    use aipl_syntax::ast::Type;
    fn again(t: &mut Type) {
        match t {
            Type::TypeVar(v) if v.starts_with(crate::pin::ANONYMOUS_ANY) => *t = Type::Any,
            Type::Case(i) | Type::Optional(i) | Type::Array(i) | Type::Set(i, _) => again(i),
            Type::Without(i, _) => again(i),
            Type::Dict(a, b) | Type::Result(a, b) => {
                again(a);
                again(b);
            }
            Type::Fn(ps, r) => {
                ps.iter_mut().for_each(again);
                again(r);
            }
            Type::Tuple(ts) | Type::Generic(_, ts) => ts.iter_mut().for_each(again),
            _ => {}
        }
    }
    sig.type_vars
        .retain(|tp| !tp.name.starts_with(crate::pin::ANONYMOUS_ANY));
    for p in &mut sig.params {
        again(&mut p.ty);
    }
    if let Some(r) = sig.return_ty.as_mut() {
        again(r);
    }
}

/// [`stamp_everything`] stamps a function's body; a `.test` body is stamped the same way by
/// standing in for the body of a program of its own.
fn stamp_test_bodies(program: &mut Program, typed: &TProgram, instances: &Instances) {
    let mut tests = Program {
        items: Vec::new(),
        sources: program.sources.clone(),
        doc: program.doc.clone(),
    };
    let mut typed_tests = typed.clone();
    typed_tests.items.retain_mut(|item| match item {
        TItem::Fn(f) => match f.test_body.take() {
            Some(test_body) => {
                f.body = test_body;
                true
            }
            None => false,
        },
        _ => true,
    });
    for item in &program.items {
        if let Item::Fn(f) = item {
            if let Some(test_body) = &f.test_body {
                let mut stand_in = f.clone();
                stand_in.body = test_body.clone();
                tests.items.push(Item::Fn(stand_in));
            }
        }
    }
    let pinned = Pinned {
        program: typed_tests,
        failures: Vec::new(),
        declared: 0,
        instances: instances.clone(),
        resolutions: Vec::new(),
    };
    stamp_everything(&mut tests, &pinned, Stamping::Typed);
    for item in tests.items {
        if let Item::Fn(stamped) = item {
            for original in &mut program.items {
                if let Item::Fn(f) = original {
                    if f.name.text == stamped.name.text {
                        f.test_body = Some(stamped.body.clone());
                    }
                }
            }
        }
    }
}
