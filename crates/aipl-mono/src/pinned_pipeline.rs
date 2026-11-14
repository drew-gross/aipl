//! What carries a pinned program through the pre-monomorphization pipeline and hands it on:
//! the rewrite that gives each `int_parse` the call its target type asks for, and writing the
//! optimized pinned functions back into the program monomorphization reads.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{Callee, Function, Item, Primitive, Program};
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

/// `original` with each function's body (and `.test` body) and each struct field default taken
/// from `typed`, the functions `typed` no longer has dropped and the ones it has gained added. The
/// generic declarations pinning leaves out stay as they are. Every expression is stamped with the
/// type `typed` gives it.
pub fn write_back(original: &Program, typed: &TProgram, instances: &Instances) -> Program {
    let erased = typed.erase();
    let mut fns: HashMap<&str, &Function> = HashMap::new();
    let mut structs = HashMap::new();
    for item in &erased.items {
        match item {
            Item::Fn(f) => {
                fns.insert(f.name.text.as_str(), f);
            }
            Item::Struct(s) => {
                structs.insert(s.name.text.as_str(), s);
            }
            _ => {}
        }
    }
    let known: HashSet<&str> = original
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) => Some(f.name.text.as_str()),
            _ => None,
        })
        .collect();
    let mut items: Vec<Item> = original
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) => {
                let new = fns.get(f.name.text.as_str())?;
                let mut f = f.clone();
                f.body = new.body.clone();
                if f.test_body.is_some() && new.test_body.is_some() {
                    f.test_body = new.test_body.clone();
                }
                Some(Item::Fn(f))
            }
            Item::Struct(s) => {
                let mut s = s.clone();
                if let Some(new) = structs.get(s.name.text.as_str()) {
                    // A default the pass folded; one pinning did not carry stays as written.
                    for (field, new) in s.fields.iter_mut().zip(&new.fields) {
                        if new.default.is_some() {
                            field.default = new.default.clone();
                        }
                    }
                }
                Some(Item::Struct(s))
            }
            other => Some(other.clone()),
        })
        .collect();
    for item in &erased.items {
        if let Item::Fn(f) = item {
            if !known.contains(f.name.text.as_str()) {
                items.push(Item::Fn(f.clone()));
            }
        }
    }
    let mut program = Program {
        items,
        sources: original.sources.clone(),
        doc: original.doc.clone(),
    };
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
