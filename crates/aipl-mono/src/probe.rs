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
    nodes: HashMap<(usize, usize, String), Option<Ty>>,
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
    /// Disagreements at a span several nodes share; see [`Probe::finish`].
    pub collisions: usize,
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
    /// Every disagreement seen: (function, span, form, mono's type, the pinned type).
    seen: RefCell<Vec<(String, (usize, usize), String, String, String)>>,
}

/// What a function's type variables are, read off the instance's parameter types.
pub type Substitution = HashMap<String, Type>;

impl Probe {
    pub fn new(pinned: &TProgram) -> Probe {
        let mut fns = HashMap::new();
        for item in &pinned.items {
            let TItem::Fn(f) = item else { continue };
            let mut nodes: HashMap<(usize, usize, String), Option<Ty>> = HashMap::new();
            f.body.walk(&mut |e| {
                let key = (e.span.start, e.span.end, typed_form(&e.kind));
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
            seen: RefCell::default(),
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
        let Some(Some(want)) =
            pinned
                .nodes
                .get(&(expr.span.start, expr.span.end, form(&expr.kind)))
        else {
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
        self.seen.borrow_mut().push((
            template.to_string(),
            (expr.span.start, expr.span.end),
            form(&expr.kind),
            mono,
            pinned,
        ));
    }

    /// The report, with the disagreements sorted: a span at which mono itself gave different types
    /// (in one function) is a span several nodes share, which says nothing about either side, and
    /// is counted apart as `collisions`.
    pub fn finish(self) -> Report {
        let mut report = self.report.into_inner();
        let seen = self.seen.into_inner();
        let mut by_span: HashMap<(&str, (usize, usize)), Vec<&str>> = HashMap::new();
        for (template, span, _, mono, _) in &seen {
            let types = by_span.entry((template, *span)).or_default();
            if !types.contains(&mono.as_str()) {
                types.push(mono);
            }
        }
        for (template, span, form, mono, pinned) in &seen {
            if by_span[&(template.as_str(), *span)].len() > 1 {
                report.collisions += 1;
                continue;
            }
            let key = format!("{form}: mono {mono}, pinned {pinned}");
            let example = format!("{template} @ {}..{}", span.0, span.1);
            report.disagreements.entry(key).or_insert((0, example)).0 += 1;
        }
        report
    }
}

/// The form of a pinned expression, spelled as [`form`] spells the one it was pinned from.
fn typed_form(kind: &aipl_syntax::typed::TExprKind<Ty>) -> String {
    if let aipl_syntax::typed::TExprKind::Call { callee, .. } = kind {
        return format!("Call {}", callee.name());
    }
    first_word(kind)
}

/// The name a derived `Debug` starts with, without formatting the rest.
fn first_word(value: &dyn std::fmt::Debug) -> String {
    struct Word(String);
    impl std::fmt::Write for Word {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            match s.find(['(', ' ', '{']) {
                Some(end) => {
                    self.0.push_str(&s[..end]);
                    Err(std::fmt::Error)
                }
                None => {
                    self.0.push_str(s);
                    Ok(())
                }
            }
        }
    }
    let mut word = Word(String::new());
    let _ = std::fmt::write(&mut word, format_args!("{value:?}"));
    word.0
}

/// The form of an expression, by name.
fn form(kind: &ExprKind) -> String {
    if let ExprKind::Call(callee, ..) = kind {
        return format!("Call {}", callee.name());
    }
    first_word(kind)
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

/// How the pinner's types compare with the stamps the checker leaves on the program
/// (`Expr::ty`), node by node.
#[derive(Default, Debug)]
pub struct StampReport {
    /// Stamped expressions with a pinned counterpart.
    pub compared: usize,
    pub agreed: usize,
    /// Stamped expressions the pinned function has no node for at that span.
    pub unmatched: usize,
    /// Every stamped expression, by form.
    pub forms: BTreeMap<String, usize>,
    /// Disagreements by form of expression and the two types: count and one example.
    pub disagreements: BTreeMap<String, (usize, String)>,
}

/// Compare every stamped expression of `stamped` (the checker's output) with the pinned type of the
/// node at the same span in the same function.
pub fn compare_stamps(stamped: &aipl_syntax::ast::Program, pinned: &TProgram) -> StampReport {
    let probe = Probe::new(pinned);
    let mut report = StampReport::default();
    for item in &stamped.items {
        let aipl_syntax::ast::Item::Fn(f) = item else {
            continue;
        };
        let Some(pf) = probe.fns.get(&f.name.text) else {
            continue;
        };
        let mut stack = vec![&f.body];
        while let Some(e) = stack.pop() {
            stack.extend(crate::children(e));
            let Some(stamp) = e.ty.as_deref() else {
                continue;
            };
            *report.forms.entry(form(&e.kind)).or_default() += 1;
            let Some(Some(want)) = pf.nodes.get(&(e.span.start, e.span.end, form(&e.kind))) else {
                report.unmatched += 1;
                let why = match pf.nodes.get(&(e.span.start, e.span.end, form(&e.kind))) {
                    Some(None) => "span shared by nodes of different types",
                    _ => "no pinned node at the span",
                };
                let key = format!("unmatched {}: {why}", form(&e.kind));
                let example = format!("{} @ {}..{}", f.name.text, e.span.start, e.span.end);
                report.disagreements.entry(key).or_insert((0, example)).0 += 1;
                continue;
            };
            report.compared += 1;
            let (got, want) = (canon(&norm(stamp)), canon(&want.to_type()));
            if got == want {
                report.agreed += 1;
                continue;
            }
            let key = format!(
                "{}: stamp {}, pinned {}",
                form(&e.kind),
                aipl_syntax::type_name(&got),
                aipl_syntax::type_name(&want)
            );
            let example = format!("{} @ {}..{}", f.name.text, e.span.start, e.span.end);
            report.disagreements.entry(key).or_insert((0, example)).0 += 1;
        }
    }
    report
}

/// A type with a generic application whose arguments are all known written as the instance it
/// names, the one spelling both sides use for it.
fn canon(t: &Type) -> Type {
    let b = |t: &Type| Box::new(canon(t));
    match t {
        Type::Generic(base, args) => {
            let args: Vec<Type> = args.iter().map(canon).collect();
            if args.iter().any(has_variable) {
                Type::Generic(base.clone(), args)
            } else {
                Type::Named(crate::check::generic_instance_name(base, &args))
            }
        }
        Type::Case(t) => Type::Case(b(t)),
        Type::Optional(t) => Type::Optional(b(t)),
        Type::Array(t) => Type::Array(b(t)),
        Type::Set(t, o) => Type::Set(b(t), *o),
        Type::Dict(k, v) => Type::Dict(b(k), b(v)),
        Type::Result(a, c) => Type::Result(b(a), b(c)),
        Type::Fn(ps, r) => Type::Fn(ps.iter().map(canon).collect(), b(r)),
        other => other.clone(),
    }
}

fn has_variable(t: &Type) -> bool {
    Ty::from_type(t).is_none_or(|t| t.mentions_var())
}

/// Experiment: give every expression the pinned type of the node at its span, where it has none
/// (`Expr::ty`), except a unit or never-returning one. With the checker's own stamps kept, mono runs on
/// a program in which every node says what it is.
pub fn stamp_everything(program: &mut aipl_syntax::ast::Program, pinned: &TProgram) {
    let probe = Probe::new(pinned);
    for item in &mut program.items {
        let aipl_syntax::ast::Item::Fn(f) = item else {
            continue;
        };
        let Some(pf) = probe.fns.get(&f.name.text) else {
            continue;
        };
        fn go(e: &mut Expr, nodes: &HashMap<(usize, usize, String), Option<Ty>>) {
            if e.ty.is_none() {
                if let Some(Some(ty)) = nodes.get(&(e.span.start, e.span.end, form(&e.kind))) {
                    if !matches!(ty, Ty::Unit | Ty::Never) {
                        e.ty = Some(Box::new(ty.to_type()));
                    }
                }
            }
            for child in crate::children_mut(e) {
                go(child, nodes);
            }
        }
        go(&mut f.body, &pf.nodes);
    }
}
