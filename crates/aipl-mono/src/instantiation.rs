//! A generic call's type arguments, read back from the types of the call's own parts.
//!
//! A call records how it instantiated its callee ([`TExprKind::Call::type_args`]). Everything
//! a call is made of is typed, so the arguments follow from the callee's signature and the types of
//! the arguments and the result: matching each declared parameter type against the type the
//! argument has binds the type variables it mentions.

use std::collections::HashMap;

use aipl_syntax::ast::{Arity, Primitive};
use aipl_syntax::typed::{TExpr, TExprKind, TItem, TProgram, TSignature, Ty};

use crate::pin::Instances;
use crate::typed_util::each_child_mut;

/// The signature of each function a call may name: the builtin declarations and the program's own
/// functions.
pub type Signatures = HashMap<String, TSignature<Ty>>;

/// The signature of every function of `program`.
pub fn signatures(program: &TProgram) -> Signatures {
    program
        .items
        .iter()
        .filter_map(|item| match item {
            TItem::Fn(f) => Some((f.name.text.clone(), f.sig.clone())),
            _ => None,
        })
        .collect()
}

/// The element a sequence parameter's `T[]` is matched with when the argument is `ty`: an array's
/// element, a set's, a dict's value, a `str`'s `char`, a range's index.
fn sequence_element(ty: &Ty) -> Option<Ty> {
    match ty {
        Ty::Array(e) | Ty::Set(e, _) | Ty::Optional(e) => Some((**e).clone()),
        Ty::Dict(_, v) => Some((**v).clone()),
        Ty::Primitive(Primitive::Str) => Some(Ty::Primitive(Primitive::Char)),
        Ty::Named(n) if n == aipl_syntax::SPAN_TYPE => Some(Ty::Primitive(Primitive::U64)),
        _ => None,
    }
}

struct Binder<'a> {
    vars: &'a [String],
    instances: &'a Instances,
    bound: HashMap<String, Ty>,
}

impl Binder<'_> {
    /// Bind the type variables `pattern` mentions by matching it against `actual`. A variable
    /// keeps the first type it is bound to.
    fn bind(&mut self, pattern: &Ty, actual: &Ty) {
        match (pattern, actual) {
            (Ty::Var(v), _) if self.vars.contains(v) => {
                self.bound
                    .entry(v.clone())
                    .or_insert_with(|| actual.clone());
            }
            (Ty::Array(p), Ty::Array(a))
            | (Ty::Optional(p), Ty::Optional(a))
            | (Ty::Case(p), Ty::Case(a))
            | (Ty::Set(p, _), Ty::Set(a, _)) => self.bind(p, a),
            // A grid stands where a sequence of its cells is asked for.
            (Ty::Array(p), Ty::Generic(base, a))
                if base == aipl_syntax::GRID_TYPE && a.len() == 1 =>
            {
                self.bind(p, &a[0]);
            }
            (Ty::Array(p), Ty::Named(instance))
                if self
                    .instances
                    .get(instance)
                    .is_some_and(|(base, a)| base == aipl_syntax::GRID_TYPE && a.len() == 1) =>
            {
                let cell = self.instances[instance].1[0].clone();
                self.bind(p, &cell);
            }
            (Ty::Array(p), other) => {
                if let Some(element) = sequence_element(other) {
                    self.bind(p, &element);
                }
            }
            (Ty::Dict(pk, pv), Ty::Dict(ak, av)) | (Ty::Result(pk, pv), Ty::Result(ak, av)) => {
                self.bind(pk, ak);
                self.bind(pv, av);
            }
            (Ty::Fn(pp, pr), Ty::Fn(ap, ar)) => {
                for (p, a) in pp.iter().zip(ap) {
                    self.bind(p, a);
                }
                self.bind(pr, ar);
            }
            (Ty::Generic(base, pargs), Ty::Named(instance)) => {
                if let Some((instance_base, aargs)) = self.instances.get(instance) {
                    if instance_base == base {
                        for (p, a) in pargs.iter().zip(aargs) {
                            self.bind(p, a);
                        }
                    }
                }
            }
            (Ty::Generic(base, pargs), Ty::Generic(abase, aargs)) if base == abase => {
                for (p, a) in pargs.iter().zip(aargs) {
                    self.bind(p, a);
                }
            }
            _ => {}
        }
    }
}

/// The type arguments of a call of `sig` whose arguments have types `args` and whose result has
/// type `result`, in the order of `sig`'s type variables. `None` when a variable is mentioned
/// nowhere the call shows.
pub fn derive_type_args(
    sig: &TSignature<Ty>,
    args: &[Ty],
    result: &Ty,
    instances: &Instances,
) -> Option<Vec<Ty>> {
    let vars: Vec<String> = sig.type_vars.iter().map(|t| t.name.clone()).collect();
    let mut binder = Binder {
        vars: &vars,
        instances,
        bound: HashMap::new(),
    };
    // Ordinary parameters first, then the result, then the repeated ones, whose arguments may be
    // the element, the sequence or an optional element.
    for (param, arg) in sig.params.iter().zip(args) {
        if param.arity == Arity::One {
            binder.bind(&param.ty, arg);
        }
    }
    if let Some(ret) = &sig.return_ty {
        binder.bind(ret, result);
    }
    for (i, param) in sig.params.iter().enumerate() {
        if param.arity != Arity::One {
            for arg in args.iter().skip(i) {
                binder.bind(&param.ty, arg);
            }
        }
    }
    vars.iter().map(|v| binder.bound.get(v).cloned()).collect()
}

/// `program` with every call's `type_args` read back from its signature. A call to a function with
/// no signature here (a local holding a function) is left as it is. The calls whose variables the
/// call does not determine, and that do not already record one argument per type variable, are
/// returned as `"in f: g"`.
pub fn reinstantiated(
    program: &TProgram,
    declared: &Signatures,
    instances: &Instances,
) -> (TProgram, Vec<String>) {
    let mut signatures = declared.clone();
    signatures.extend(self::signatures(program));
    let mut out = program.clone();
    let mut unresolved = Vec::new();
    for item in &mut out.items {
        if let TItem::Fn(f) = item {
            let owner = f.name.text.clone();
            let mut visit = |e: &mut TExpr<Ty>| {
                reinstantiate(e, &owner, &signatures, instances, &mut unresolved)
            };
            visit(&mut f.body);
            if let Some(test_body) = &mut f.test_body {
                visit(test_body);
            }
        }
    }
    (out, unresolved)
}

fn reinstantiate(
    e: &mut TExpr<Ty>,
    owner: &str,
    signatures: &Signatures,
    instances: &Instances,
    unresolved: &mut Vec<String>,
) {
    each_child_mut(e, &mut |c| {
        reinstantiate(c, owner, signatures, instances, unresolved)
    });
    let TExprKind::Call {
        callee,
        type_args,
        args,
        ..
    } = &mut e.kind
    else {
        return;
    };
    let Some(sig) = signatures.get(callee.name()) else {
        return;
    };
    if sig.type_vars.is_empty() {
        type_args.clear();
        return;
    }
    let arg_types: Vec<Ty> = args.iter().map(|a| a.ty.clone()).collect();
    match derive_type_args(sig, &arg_types, &e.ty, instances) {
        Some(derived) => *type_args = derived,
        // A type the parts show only as a lowered instance name (`Box$i64`) cannot be taken
        // apart; a call that already records one argument per variable keeps them.
        None if type_args.len() == sig.type_vars.len() => {}
        None => unresolved.push(format!("in {owner}: {}", callee.name())),
    }
}
