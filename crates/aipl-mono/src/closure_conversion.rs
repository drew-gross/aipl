//! Function values that capture, made into data.
//!
//! A lambda used as a value (stored in a variant or struct, bound by a `let`) keeps the locals it
//! reads. Those arrive here as a call to `__closure$<lifted function>(captures..)`, made by
//! monomorphization once the lambda has been lifted. For each function type that has one, every
//! value of that type becomes a synthesized variant `Closure$<n>` with one case per function that
//! can stand for it, the case holding the captures; a call through such a value becomes a call to
//! `apply$<n>`, which matches on the case and calls the function with the captures appended.
//!
//! This is the whole of it: a closure is an ordinary variant to everything after, so its layout,
//! copying and release are the variant's. A function type with no capturing lambda is left as the
//! code address it has always been.

use std::collections::HashMap;

use aipl_syntax::ast::{Callee, ConcreteType};
use aipl_syntax::ast::{ConcreteStructDecl, ConcreteVariantCase, ConcreteVariantDecl};
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TPattern};
use aipl_syntax::Span;

use crate::typed_util::{each_child_mut, Typed};
use crate::{ConcreteFn, ConcreteParam};

/// What a lambda used as a value is replaced by before this pass: a call to this name followed by
/// the lifted function's name.
pub(crate) const MAKE_PREFIX: &str = "__closure$";

/// One function type being made into a variant.
struct Signature {
    params: Vec<ConcreteType>,
    ret: ConcreteType,
    variant: String,
    apply: String,
    /// The functions that can stand for a value of the type, with what each captures.
    cases: Vec<Case>,
}

struct Case {
    function: String,
    captures: Vec<ConcreteType>,
}

impl Case {
    fn name(&self) -> String {
        format!("Fn${}", self.function)
    }
}

pub(crate) struct Conversion {
    signatures: Vec<Signature>,
    /// The functions of the program with the effects each performs: what an identifier of
    /// function type may name.
    functions: HashMap<String, Vec<String>>,
}

impl Conversion {
    /// The conversion a program needs, or `None` when no lambda in it captures.
    pub(crate) fn find(fns: &[ConcreteFn]) -> Option<Conversion> {
        let mut signatures: Vec<Signature> = Vec::new();
        for f in fns {
            find_makers(&f.body, &mut signatures);
        }
        if signatures.is_empty() {
            return None;
        }
        Some(Conversion {
            signatures,
            functions: fns
                .iter()
                .map(|f| (f.name.clone(), f.effects.clone()))
                .collect(),
        })
    }

    fn signature_of(&self, ty: &ConcreteType) -> Option<usize> {
        let ConcreteType::Fn(params, ret, _, _) = ty else {
            return None;
        };
        self.signatures
            .iter()
            .position(|s| s.params == *params && s.ret == **ret)
    }

    /// `ty` with every converted function type replaced by its variant.
    fn map_type(&self, ty: &ConcreteType) -> ConcreteType {
        let map = |t: &ConcreteType| Box::new(self.map_type(t));
        match ty {
            ConcreteType::Fn(params, ret, effects, mutating) => match self.signature_of(ty) {
                Some(i) => ConcreteType::Named(self.signatures[i].variant.clone()),
                None => ConcreteType::Fn(
                    params.iter().map(|p| self.map_type(p)).collect(),
                    map(ret),
                    effects.clone(),
                    *mutating,
                ),
            },
            ConcreteType::Optional(t) => ConcreteType::Optional(map(t)),
            ConcreteType::Array(t) => ConcreteType::Array(map(t)),
            ConcreteType::Set(t, order) => ConcreteType::Set(map(t), *order),
            ConcreteType::Dict(k, v) => ConcreteType::Dict(map(k), map(v)),
            ConcreteType::Result(ok, err) => ConcreteType::Result(map(ok), map(err)),
            ConcreteType::Unit
            | ConcreteType::Primitive(_)
            | ConcreteType::Named(_)
            | ConcreteType::Case(_)
            | ConcreteType::ConcatStr => ty.clone(),
        }
    }

    /// Every function and declaration rewritten, and the variants and `apply` functions added.
    pub(crate) fn apply(
        mut self,
        fns: &mut Vec<ConcreteFn>,
        structs: &mut [ConcreteStructDecl],
        variants: &mut Vec<ConcreteVariantDecl>,
    ) {
        for f in fns.iter_mut() {
            self.rewrite(&mut f.body);
        }
        let this = &self;
        for f in fns.iter_mut() {
            for p in &mut f.params {
                p.ty = this.map_type(&p.ty);
            }
            f.return_ty = f.return_ty.as_ref().map(|t| this.map_type(t));
        }
        for s in structs.iter_mut() {
            for field in &mut s.fields {
                field.ty = this.map_type(&field.ty);
            }
        }
        for v in variants.iter_mut() {
            for case in &mut v.cases {
                for p in &mut case.payload {
                    *p = this.map_type(p);
                }
            }
        }
        for s in &this.signatures {
            variants.push(ConcreteVariantDecl {
                name: s.variant.clone(),
                cases: s
                    .cases
                    .iter()
                    .map(|c| ConcreteVariantCase {
                        name: c.name(),
                        payload: c.captures.iter().map(|t| this.map_type(t)).collect(),
                    })
                    .collect(),
            });
            fns.push(this.apply_function(s));
        }
    }

    fn case_index(&mut self, signature: usize, function: &str, captures: &[ConcreteType]) -> usize {
        let cases = &mut self.signatures[signature].cases;
        match cases.iter().position(|c| c.function == function) {
            Some(i) => i,
            None => {
                cases.push(Case {
                    function: function.to_string(),
                    captures: captures.to_vec(),
                });
                cases.len() - 1
            }
        }
    }

    fn rewrite(&mut self, e: &mut Typed) {
        // What the node is is read from the types it was given, before they are replaced.
        let original = e.ty.clone();
        let call_signature = match &e.kind {
            TExprKind::Call {
                callee: Callee::Local(_),
                args,
                ..
            } => {
                let probe = ConcreteType::Fn(
                    args.iter().map(|a| a.ty.clone()).collect(),
                    Box::new(e.ty.clone()),
                    Vec::new(),
                    false,
                );
                self.signature_of(&probe)
            }
            _ => None,
        };
        each_child_mut(e, &mut |child| self.rewrite(child));
        e.ty = self.map_type(&e.ty);
        e.annotation = e.annotation.as_ref().map(|t| self.map_type(t));
        if let TExprKind::Call { type_args, .. } = &mut e.kind {
            for t in type_args {
                *t = self.map_type(t);
            }
        }
        if let TExprKind::Lambda(params, _) = &mut e.kind {
            for p in params {
                p.ty = self.map_type(&p.ty);
            }
        }
        match &mut e.kind {
            TExprKind::Call {
                callee: Callee::User(name),
                args,
                ..
            } if name.starts_with(MAKE_PREFIX) => {
                let function = name[MAKE_PREFIX.len()..].to_string();
                let signature = self.signature_of(&original).expect("found by `find`");
                let captures: Vec<ConcreteType> = args.iter().map(|a| a.ty.clone()).collect();
                let index = self.case_index(signature, &function, &captures);
                let case = self.signatures[signature].cases[index].name();
                let variant = self.signatures[signature].variant.clone();
                let args = std::mem::take(args);
                e.kind = if args.is_empty() {
                    TExprKind::Ident(format!("{case}@{variant}"))
                } else {
                    TExprKind::Call {
                        callee: Callee::User(format!("{case}@{variant}")),
                        type_args: Vec::new(),
                        args,
                        method_style: false,
                    }
                };
            }
            TExprKind::Ident(name) => {
                if let Some(signature) = self.signature_of(&original) {
                    if self.functions.contains_key(name) {
                        // A function named as a value captures nothing: a lambda that does is
                        // made through `__closure$`.
                        let function = name.clone();
                        let index = self.case_index(signature, &function, &[]);
                        let case = self.signatures[signature].cases[index].name();
                        let variant = self.signatures[signature].variant.clone();
                        e.kind = TExprKind::Ident(format!("{case}@{variant}"));
                    }
                }
            }
            TExprKind::Call {
                callee,
                args,
                method_style,
                ..
            } if call_signature.is_some() => {
                let Callee::Local(local) = callee.clone() else {
                    unreachable!("matched above");
                };
                let signature = &self.signatures[call_signature.expect("checked")];
                let closure = TExpr::new(
                    TExprKind::Ident(local),
                    ConcreteType::Named(signature.variant.clone()),
                    e.span.clone(),
                );
                args.insert(0, closure);
                *callee = Callee::User(signature.apply.clone());
                *method_style = false;
            }
            _ => {}
        }
    }

    /// `apply$<n>(closure, args..)`: the call of whichever function the closure is.
    fn apply_function(&self, s: &Signature) -> ConcreteFn {
        let span = Span::default();
        let closure_ty = ConcreteType::Named(s.variant.clone());
        let argument_names: Vec<String> = (0..s.params.len()).map(|i| format!("a{i}")).collect();
        let node = |kind: TExprKind<ConcreteType>, ty: &ConcreteType| {
            TExpr::new(kind, ty.clone(), span.clone())
        };
        let ident = |name: &str, ty: &ConcreteType| node(TExprKind::Ident(name.to_string()), ty);
        let mut params = vec![ConcreteParam {
            name: "closure".to_string(),
            ty: closure_ty.clone(),
            mutable: false,
            owned: false,
        }];
        params.extend(
            argument_names
                .iter()
                .zip(&s.params)
                .map(|(name, ty)| ConcreteParam {
                    name: name.clone(),
                    ty: self.map_type(ty),
                    mutable: false,
                    owned: false,
                }),
        );
        let ret = self.map_type(&s.ret);
        let arguments = || -> Vec<Typed> {
            argument_names
                .iter()
                .zip(&s.params)
                .map(|(name, ty)| ident(name, &self.map_type(ty)))
                .collect()
        };
        let arms = s
            .cases
            .iter()
            .map(|case| {
                let capture_names: Vec<String> =
                    (0..case.captures.len()).map(|i| format!("c{i}")).collect();
                let body = {
                    let mut args = arguments();
                    args.extend(
                        capture_names
                            .iter()
                            .zip(&case.captures)
                            .map(|(name, ty)| ident(name, &self.map_type(ty))),
                    );
                    node(
                        TExprKind::Call {
                            callee: Callee::User(case.function.clone()),
                            type_args: Vec::new(),
                            args,
                            method_style: false,
                        },
                        &ret,
                    )
                };
                TMatchArm {
                    pattern: TPattern::Ctor {
                        name: case.name(),
                        bindings: capture_names,
                        ignore_payload: false,
                    },
                    body,
                    span: span.clone(),
                }
            })
            .collect();
        // It performs whatever any of the functions it calls does.
        let mut effects: Vec<String> = s
            .cases
            .iter()
            .flat_map(|c| self.functions[&c.function].iter().cloned())
            .collect();
        effects.sort();
        effects.dedup();
        ConcreteFn {
            name: s.apply.clone(),
            params,
            effects,
            effects_by_type: true,
            return_ty: Some(ret.clone()),
            body: node(
                TExprKind::Match(Box::new(ident("closure", &closure_ty)), arms),
                &ret,
            ),
        }
    }
}

/// The function types that have a `__closure$` call in `e`, added to `signatures`.
fn find_makers(e: &Typed, signatures: &mut Vec<Signature>) {
    let is_maker = matches!(
        &e.kind,
        TExprKind::Call { callee: Callee::User(name), .. } if name.starts_with(MAKE_PREFIX)
    );
    if let ConcreteType::Fn(params, ret, _, _) = &e.ty {
        if is_maker {
            match signatures
                .iter_mut()
                .find(|s| s.params == *params && s.ret == **ret)
            {
                Some(_) => {}
                None => {
                    let n = signatures.len();
                    signatures.push(Signature {
                        params: params.clone(),
                        ret: (**ret).clone(),
                        variant: format!("Closure${n}"),
                        apply: format!("apply${n}"),
                        cases: Vec::new(),
                    });
                }
            }
        }
    }
    crate::typed_util::each_child(e, &mut |c| find_makers(c, signatures));
}
