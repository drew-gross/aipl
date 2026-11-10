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

use aipl_syntax::ast::{Callee, Expr, ExprKind, Type};
use aipl_syntax::typed::{ConcreteTy, TExpr, TItem, TProgram, Ty, TyRepr};

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
            Some(crate::typing::generic_instance_name(base, &written))
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
    // The numeral is `Num` in the written tree.
    match first_word(kind).as_str() {
        "Int" => "Num".to_string(),
        word => word.to_string(),
    }
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
                Type::Named(crate::typing::generic_instance_name(base, &args))
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

/// What [`stamp_everything`] does with a node that already carries a stamp.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stamping {
    /// Leave it: the stamp was recorded where the context was in view.
    KeepExisting,
    /// Overwrite it with the pinned type: the tree was pinned after the passes that moved it.
    Replace,
}

/// Give every expression the pinned type of the node it was pinned as, where it has none
/// (`Expr::ty`), except a unit or never-returning one. `stamping` says whether a stamp already there is kept.
///
/// The two trees are walked together, since pinning does not restructure anything. Where they stop
/// matching (a node of another form, a different number of children) the rest of that subtree is
/// matched by span and form instead, and left alone if several pinned nodes share them: a default
/// spliced into several calls keeps one span and takes a different type at each.
pub fn stamp_everything(
    program: &mut aipl_syntax::ast::Program,
    pinned: &crate::pin::Pinned,
    stamping: Stamping,
) {
    let probe = Probe::new(&pinned.program);
    let bodies: HashMap<&str, &TExpr<Ty>> = pinned
        .program
        .items
        .iter()
        .filter_map(|item| match item {
            TItem::Fn(f) => Some((f.name.text.as_str(), &f.body)),
            _ => None,
        })
        .collect();
    for item in &mut program.items {
        let aipl_syntax::ast::Item::Fn(f) = item else {
            continue;
        };
        let (Some(pf), Some(body)) = (
            probe.fns.get(&f.name.text),
            bodies.get(f.name.text.as_str()),
        ) else {
            continue;
        };
        let stamper = Stamper {
            instances: &pinned.instances,
            nodes: &pf.nodes,
            stamping,
        };
        strip_type_args(&mut f.body);
        stamper.together(&mut f.body, body);
        retarget_int_parses(&mut f.body);
    }
}

/// Written type arguments are spent once the call's type is known (the pinner records it), so the
/// call stands alone: `f<T>(x)` is `f(x)`.
fn strip_type_args(e: &mut Expr) {
    while let ExprKind::TypeArgs(inner, _) = &mut e.kind {
        let inner = std::mem::replace(&mut **inner, Expr::new(ExprKind::Unit, 0..0));
        *e = inner;
    }
    for child in crate::children_mut(e) {
        strip_type_args(child);
    }
}

/// An `int_parse` stamped as producing an integer other than `i64`, as the calls that produce it:
/// the signed parse for a signed target, the unsigned one for an unsigned target (so a `u64`
/// reaches past `i64::MAX`), each narrowed to the target unless it already is one.
fn retarget_int_parses(e: &mut Expr) {
    for child in crate::children_mut(e) {
        retarget_int_parses(child);
    }
    let ExprKind::Call(Callee::IntParse, args, style) = &e.kind else {
        return;
    };
    let Some(Type::Optional(core)) = e.ty.as_deref() else {
        return;
    };
    let Type::Primitive(target) = **core else {
        return;
    };
    let parse = if target.int_signed() {
        Callee::IntParse
    } else {
        Callee::IntParseUnsigned
    };
    let mut call = Expr::new(ExprKind::Call(parse, args.clone(), *style), e.span.clone());
    if matches!(
        target,
        aipl_syntax::ast::Primitive::I64 | aipl_syntax::ast::Primitive::U64
    ) {
        call.ty = e.ty.clone();
        *e = call;
        return;
    }
    let mut narrowed = Expr::new(
        ExprKind::Call(Callee::Narrow, vec![call], true),
        e.span.clone(),
    );
    narrowed.ty = e.ty.clone();
    *e = narrowed;
}

/// Whether `t` names the type variable of an anonymous `any` (see [`crate::pin::ANONYMOUS_ANY`]).
fn mentions_anonymous_any(t: &Type) -> bool {
    match t {
        Type::TypeVar(v) => v.starts_with(crate::pin::ANONYMOUS_ANY),
        Type::Case(t) | Type::Optional(t) | Type::Array(t) | Type::Set(t, _) => {
            mentions_anonymous_any(t)
        }
        Type::Dict(a, b) | Type::Result(a, b) => {
            mentions_anonymous_any(a) || mentions_anonymous_any(b)
        }
        Type::Fn(ps, r) => ps.iter().any(mentions_anonymous_any) || mentions_anonymous_any(r),
        Type::Generic(_, args) | Type::Tuple(args) => args.iter().any(mentions_anonymous_any),
        _ => false,
    }
}

/// [`stamp_everything`]'s walk over one function body.
struct Stamper<'a> {
    instances: &'a crate::pin::Instances,
    nodes: &'a HashMap<(usize, usize, String), Option<Ty>>,
    stamping: Stamping,
}

impl Stamper<'_> {
    fn stamp(&self, e: &mut Expr, ty: &Ty) {
        if matches!(ty, Ty::Unit | Ty::Never) {
            return;
        }
        let pinned = self.spelled(ty.to_type());
        if mentions_anonymous_any(&pinned) {
            return;
        }
        match (&e.ty, self.stamping) {
            // A stamp that already says this keeps its spelling.
            (Some(old), Stamping::Replace) if canon(old) == canon(&pinned) => {}
            (Some(_), Stamping::KeepExisting) => {}
            _ => e.ty = Some(Box::new(pinned)),
        }
    }

    /// `t` with each generic instance written as the application it is. Monomorphization
    /// instantiates a generic type from the application; the instance's name alone does not say
    /// which template and arguments it came from.
    fn spelled(&self, t: Type) -> Type {
        let go = |t: Type| Box::new(self.spelled(t));
        match t {
            Type::Named(n) => match self.instances.get(&n) {
                Some((base, args)) => Type::Generic(
                    base.clone(),
                    args.iter().map(|a| self.spelled(a.to_type())).collect(),
                ),
                None => Type::Named(n),
            },
            Type::Case(t) => Type::Case(go(*t)),
            Type::Optional(t) => Type::Optional(go(*t)),
            Type::Array(t) => Type::Array(go(*t)),
            Type::Set(t, o) => Type::Set(go(*t), o),
            Type::Dict(k, v) => Type::Dict(go(*k), go(*v)),
            Type::Result(a, b) => Type::Result(go(*a), go(*b)),
            Type::Fn(ps, r) => Type::Fn(ps.into_iter().map(|p| self.spelled(p)).collect(), go(*r)),
            other => other,
        }
    }

    /// Stamp `e` and its subtree by span and form, for a subtree that no longer lines up with
    /// the pinned one.
    fn by_span(&self, e: &mut Expr) {
        if let Some(Some(ty)) = self.nodes.get(&(e.span.start, e.span.end, form(&e.kind))) {
            self.stamp(e, ty);
        }
        for child in crate::children_mut(e) {
            self.by_span(child);
        }
    }

    /// Stamp `e` and its subtree from the pinned `t`, the two walked together.
    fn together(&self, e: &mut Expr, t: &TExpr<Ty>) {
        if e.span != t.span || form(&e.kind) != typed_form(&t.kind) {
            return self.by_span(e);
        }
        // A generic application the pinner had to guess an argument of is monomorphization's to settle.
        if !t.unsettled {
            self.stamp(e, &t.ty);
        }
        // A value lambda's inferred parameter types become annotations, so the lambda reads
        // downstream exactly as if they had been written.
        if let (
            ExprKind::Lambda(params, _),
            aipl_syntax::typed::TExprKind::Lambda(typed_params, _),
        ) = (&mut e.kind, &t.kind)
        {
            for (p, tp) in params.iter_mut().zip(typed_params) {
                if p.ty.is_none() && !tp.ty.mentions_var() {
                    p.ty = Some(self.spelled(tp.ty.to_type()));
                }
            }
        }
        let mut typed = Vec::new();
        t.for_each_child(&mut |c| typed.push(c));
        let children: Vec<&mut Expr> = crate::children_mut(e).collect();
        if children.len() != typed.len() {
            for child in children {
                self.by_span(child);
            }
            return;
        }
        for (child, tc) in children.into_iter().zip(typed) {
            self.together(child, tc);
        }
    }
}
