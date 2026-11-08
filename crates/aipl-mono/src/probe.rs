//! Does monomorphization's own inference agree with the pinner?
//!
//! Stage 2 of `TYPED_IR.md` replaces `Mono::infer`'s types with the pinner's. Before anything is
//! rewritten, this measures the gap: while a program is monomorphized, every expression `infer`
//! types is looked up in the pinned program by function and span, the pinned type is made concrete
//! with the instance's type arguments, and the two are compared.
//!
//! It is a measurement, not a guard: a span can be shared by several nodes (a desugaring reuses the
//! span of what it replaced), and mono types rewritten trees, so a disagreement is a lead to look at
//! and an agreement is evidence. The program it runs on is not optimized, so the trees are the ones
//! the pinner saw.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use aipl_syntax::ast::{Expr, ExprKind, Type};
use aipl_syntax::typed::{ConcreteTy, TItem, TProgram, Ty, TyRepr};

/// The pinned types of one function: its parameters, and each node's type by span.
struct PinnedFn {
    params: Vec<Ty>,
    /// `None` when several nodes with different types share the span.
    nodes: HashMap<(usize, usize), Option<Ty>>,
}

/// What the comparison found.
#[derive(Default, Debug)]
pub struct Report {
    /// Expressions mono typed that have a pinned counterpart.
    pub compared: usize,
    pub agreed: usize,
    /// Expressions with no pinned counterpart: synthesized by a pass or by mono itself.
    pub unmatched: usize,
    /// Nodes whose pinned type mentions a type variable the instance's parameters do not bind.
    pub unbound: usize,
    /// Nodes where mono's type still holds a placeholder (`none`'s inner type, an empty array).
    pub placeholder: usize,
    /// Instances compared, and those skipped because their parameters would not line up.
    pub instances: usize,
    /// Disagreements grouped by the form of the expression and the two types: count and one
    /// example (`function @ span`).
    pub disagreements: BTreeMap<String, (usize, String)>,
}

pub struct Probe {
    fns: HashMap<String, PinnedFn>,
    pub report: RefCell<Report>,
}

/// What a function's type variables are, read off the instance's parameter types.
pub type Substitution = HashMap<String, Type>;

impl Probe {
    pub fn new(pinned: &TProgram) -> Probe {
        let mut fns = HashMap::new();
        for item in &pinned.items {
            let TItem::Fn(f) = item else { continue };
            let mut nodes: HashMap<(usize, usize), Option<Ty>> = HashMap::new();
            f.body.walk(&mut |e| {
                let key = (e.span.start, e.span.end);
                match nodes.get(&key) {
                    Some(Some(existing)) if *existing != e.ty => {
                        nodes.insert(key, None);
                    }
                    Some(_) => {}
                    None => {
                        nodes.insert(key, Some(e.ty.clone()));
                    }
                }
            });
            fns.insert(
                f.name.text.clone(),
                PinnedFn {
                    params: f.sig.params.iter().map(|p| p.ty.clone()).collect(),
                    nodes,
                },
            );
        }
        Probe {
            fns,
            report: RefCell::default(),
        }
    }

    /// The substitution an instance of `template` with parameters of the types `params` implies, or
    /// `None` when `template` is not a pinned function.
    pub fn substitution(&self, template: &str, params: &[Type]) -> Option<Substitution> {
        let pinned = self.fns.get(template)?;
        let mut out = Substitution::new();
        for (declared, actual) in pinned.params.iter().zip(params) {
            bind(declared, actual, &mut out);
        }
        self.report.borrow_mut().instances += 1;
        Some(out)
    }

    /// Compare mono's type for `expr` (inside `template`) with the pinned one.
    pub fn compare(&self, template: &str, subst: &Substitution, expr: &Expr, inferred: &Type) {
        let Some(pinned) = self.fns.get(template) else {
            return;
        };
        let mut report = self.report.borrow_mut();
        let Some(Some(want)) = pinned.nodes.get(&(expr.span.start, expr.span.end)) else {
            report.unmatched += 1;
            return;
        };
        if matches!(want, Ty::Never) {
            return;
        }
        let instance = |base: &str, args: &[ConcreteTy]| {
            let written: Vec<Type> = args.iter().map(|a| a.to_type()).collect();
            Some(crate::check::generic_instance_name(base, &written))
        };
        let Some(want) = want.concretize_with(
            &|v| {
                subst
                    .get(v)
                    .and_then(|t| Ty::from_type(&norm(t))?.concretize(&|_| None))
            },
            &instance,
        ) else {
            report.unbound += 1;
            return;
        };
        let Some(got) = Ty::from_type(&norm(inferred)).and_then(|t| t.concretize(&|_| None)) else {
            report.placeholder += 1;
            return;
        };
        report.compared += 1;
        if got == want {
            report.agreed += 1;
            return;
        }
        let (mono, pinned) = (
            aipl_syntax::type_name(&got.to_type()),
            aipl_syntax::type_name(&want.to_type()),
        );
        let key = format!("{}: mono {mono}, pinned {pinned}", form(&expr.kind));
        let example = format!("{template} @ {}..{}", expr.span.start, expr.span.end);
        let entry = report.disagreements.entry(key).or_insert((0, example));
        entry.0 += 1;
    }
}

/// The form of an expression, by name.
fn form(kind: &ExprKind) -> String {
    let text = format!("{kind:?}");
    text.split(['(', ' ', '{']).next().unwrap_or("").to_string()
}

/// A type with what pinned types leave out taken away: a refinement is its base and a concat string
/// is a string.
fn norm(t: &Type) -> Type {
    let b = |t: &Type| Box::new(norm(t));
    match t {
        Type::Without(t, _) => norm(t),
        Type::ConcatStr => Type::Primitive(aipl_syntax::ast::Primitive::Str),
        Type::Case(t) => Type::Case(b(t)),
        Type::Optional(t) => Type::Optional(b(t)),
        Type::Array(t) => Type::Array(b(t)),
        Type::Set(t, o) => Type::Set(b(t), *o),
        Type::Dict(k, v) => Type::Dict(b(k), b(v)),
        Type::Result(a, c) => Type::Result(b(a), b(c)),
        Type::Fn(ps, r) => Type::Fn(ps.iter().map(norm).collect(), b(r)),
        other => other.clone(),
    }
}

/// Record what each type variable of `declared` is, by reading it off `actual`. Where the two do
/// not have the same shape (a variadic retyped to its element, a string kept as a string) nothing is
/// recorded, and the variable stays unbound.
fn bind(declared: &Ty, actual: &Type, out: &mut Substitution) {
    match (declared, actual) {
        (Ty::Var(v), t) => {
            out.entry(v.clone()).or_insert_with(|| t.clone());
        }
        (Ty::Optional(d), Type::Optional(a))
        | (Ty::Array(d), Type::Array(a))
        | (Ty::Case(d), Type::Case(a))
        | (Ty::Set(d, _), Type::Set(a, _)) => bind(d, a, out),
        (Ty::Dict(dk, dv), Type::Dict(ak, av)) | (Ty::Result(dk, dv), Type::Result(ak, av)) => {
            bind(dk, ak, out);
            bind(dv, av, out);
        }
        (Ty::Fn(dp, dr), Type::Fn(ap, ar)) => {
            for (d, a) in dp.iter().zip(ap) {
                bind(d, a, out);
            }
            bind(dr, ar, out);
        }
        _ => {}
    }
}
