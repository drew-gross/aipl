//! Keyword-argument expansion: rewrite every call so that keyword arguments
//! (`f(1, k = 2)`) and omitted keyword parameters (filled from their declared
//! defaults) become plain positional arguments.
//!
//! A parameter declared with a default (`fn f(a: i64, k: i64 = 3)`) is a
//! *keyword* parameter: it must come after every positional parameter, may only
//! be supplied by keyword at a call site, and is not part of the function's
//! type. This pass runs at the end of [`flatten`] — after names are resolved
//! through each file's import view, so callees are looked up by their final
//! (mangled) names — and it removes every [`ExprKind::KwArg`] node, so no later
//! pass (checker, monomorphizer, codegen) ever sees one.
//!
//! A default expression may itself call a function with keyword parameters, so
//! defaults are expanded recursively (memoized per function, with cycle
//! detection — `fn a(x: i64 = b())` / `fn b(y: i64 = a())` is an error).
//!
//! [`flatten`]: super::Loader::flatten

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{
    Expr, ExprKind, FieldInit, Function, Item, LambdaParam, MatchArm, Program, Signature,
};
use aipl_syntax::{Error, Span};

/// Expand keyword arguments (and fill omitted keyword parameters from their
/// defaults) in every function body, `.test` body, and default expression of
/// `program`, returning the rewritten program. Errors on any misuse: a
/// positional parameter following a keyword parameter, keyword arguments to a
/// function without that keyword parameter, duplicate/positional-after-keyword
/// arguments, or a function with keyword parameters used as a value.
pub(crate) fn expand_keyword_args(program: &Program) -> Result<Program, Error> {
    // Builtins with keyword parameters (currently just `execute_program`'s
    // `args`) participate too: their calls are already rewritten to canonical
    // `__builtin_*` names by this point, and the info comes straight from the
    // single source of truth, `BUILTIN_SIGNATURES`. Seed them first; a user
    // item can never shadow a reserved `__builtin_*` name.
    let mut fns: HashMap<String, FnKwInfo> = builtin_kw_infos()?;
    for item in &program.items {
        let Item::Fn(f) = item else { continue };
        fns.insert(f.name.clone(), FnKwInfo::from_sig(&f.name, &f.sig)?);
    }

    let mut cx = Expander {
        fns,
        defaults: HashMap::new(),
        expanding: Vec::new(),
    };

    let items = program
        .items
        .iter()
        .map(|item| {
            let Item::Fn(f) = item else {
                return Ok(item.clone());
            };
            // Parameters are the body's locals: a function-typed parameter
            // named like a global function must not have that global's
            // keyword machinery applied to calls through it.
            let locals: HashSet<String> = f.sig.params.iter().map(|p| p.name.clone()).collect();
            let mut sig = f.sig.clone();
            // Store each keyword parameter's *expanded* default back into the
            // signature, so the checker never sees an unexpanded call inside
            // one.
            let expanded = cx.expanded_defaults(&f.name)?;
            for (p, d) in sig
                .params
                .iter_mut()
                .filter(|p| p.default.is_some())
                .zip(expanded)
            {
                p.default = Some(d);
            }
            Ok(Item::Fn(Function {
                name: f.name.clone(),
                is_pub: f.is_pub,
                sig,
                body: cx.expand_expr(&f.body, &locals)?,
                // A `.test` body has no parameters in scope — only globals.
                test_body: f
                    .test_body
                    .as_ref()
                    .map(|tb| cx.expand_expr(tb, &HashSet::new()))
                    .transpose()?,
                doc: f.doc.clone(),
            }))
        })
        .collect::<Result<_, _>>()?;
    Ok(Program { items })
}

/// What the expansion needs to know about one function: how many positional
/// parameters it declares, and its keyword parameters (name + declared
/// default), in declaration order.
struct FnKwInfo {
    /// The positional parameters' names (for the "passed a positional
    /// parameter by keyword" error); their count is the required positional
    /// argument count.
    positional: Vec<String>,
    kw: Vec<(String, Expr)>,
}

impl FnKwInfo {
    /// Split `sig`'s parameters into positional and keyword, enforcing the
    /// declaration rule: once a parameter has a default, every later one must
    /// too (keyword parameters come last).
    fn from_sig(name: &str, sig: &Signature) -> Result<FnKwInfo, Error> {
        let mut kw: Vec<(String, Expr)> = Vec::new();
        let mut positional = Vec::new();
        for p in &sig.params {
            match &p.default {
                Some(d) => kw.push((p.name.clone(), d.clone())),
                None => {
                    if let Some((kw_name, kw_default)) = kw.last() {
                        return Err(Error::at(
                            format!(
                                "fn {:?}: parameter {:?} has no default but follows keyword \
                                 parameter {kw_name:?}; parameters with defaults must come last",
                                display(name),
                                p.name
                            ),
                            kw_default.span.clone(),
                        ));
                    }
                    positional.push(p.name.clone());
                }
            }
        }
        Ok(FnKwInfo { positional, kw })
    }
}

/// Keyword-parameter info for every builtin that declares one, keyed by the
/// canonical `__builtin_*` name its calls carry after loader rewriting. Parsed
/// from `BUILTIN_SIGNATURES` so a builtin gains a keyword parameter simply by
/// declaring a default there — no second list to keep in sync. Builtins with no
/// defaulted parameter are skipped (an all-positional call needs no info).
fn builtin_kw_infos() -> Result<HashMap<String, FnKwInfo>, Error> {
    let program = aipl_parser::parse(aipl_syntax::BUILTIN_SIGNATURES)
        .expect("builtin signatures are valid AIPL");
    let mut map = HashMap::new();
    for item in &program.items {
        let Item::Fn(f) = item else { continue };
        if f.sig.params.iter().any(|p| p.default.is_some()) {
            map.insert(f.name.clone(), FnKwInfo::from_sig(&f.name, &f.sig)?);
        }
    }
    Ok(map)
}

struct Expander {
    fns: HashMap<String, FnKwInfo>,
    /// Memoized *expanded* default expressions per function (in keyword
    /// parameter order) — a default may itself call functions with keyword
    /// parameters.
    defaults: HashMap<String, Vec<Expr>>,
    /// Functions whose defaults are currently being expanded, for cycle
    /// detection (a stack, so the error can show the cycle).
    expanding: Vec<String>,
}

impl Expander {
    /// The expanded default expressions of `name`'s keyword parameters, in
    /// declaration order. Memoized; errors on a cycle of defaults.
    fn expanded_defaults(&mut self, name: &str) -> Result<Vec<Expr>, Error> {
        if let Some(d) = self.defaults.get(name) {
            return Ok(d.clone());
        }
        if self.expanding.iter().any(|n| n == name) {
            let cycle: Vec<&str> = self
                .expanding
                .iter()
                .map(|n| display(n))
                .chain([display(name)])
                .collect();
            return Err(Error::msg(format!(
                "cycle in keyword-parameter defaults: {}",
                cycle.join(" -> ")
            )));
        }
        self.expanding.push(name.to_string());
        // Defaults can't reference the function's parameters (they are checked
        // in an empty environment), so no locals are in scope.
        let expanded: Result<Vec<Expr>, Error> = self
            .fns
            .get(name)
            .map(|info| info.kw.iter().map(|(_, d)| d.clone()).collect::<Vec<_>>())
            .unwrap_or_default()
            .iter()
            .map(|d| self.expand_expr(d, &HashSet::new()))
            .collect();
        self.expanding.pop();
        let expanded = expanded?;
        self.defaults.insert(name.to_string(), expanded.clone());
        Ok(expanded)
    }

    /// Rewrite one call's argument list to plain positional form: validate the
    /// keyword arguments against `name`'s keyword parameters and fill each
    /// omitted one from its (expanded) default. `args` has already been
    /// expanded recursively; `span` is the call's, for errors.
    fn expand_call_args(
        &mut self,
        name: &str,
        args: Vec<Expr>,
        span: &Span,
    ) -> Result<Vec<Expr>, Error> {
        // Split the positional prefix from the keyword tail, rejecting a
        // positional argument after a keyword one.
        let mut positional: Vec<Expr> = Vec::new();
        let mut by_kw: Vec<(String, Expr, Span)> = Vec::new();
        for arg in args {
            match arg.kind {
                ExprKind::KwArg(k, v) => by_kw.push((k, *v, arg.span)),
                _ if by_kw.is_empty() => positional.push(arg),
                _ => {
                    return Err(Error::at(
                        "positional argument after a keyword argument".to_string(),
                        arg.span,
                    ));
                }
            }
        }

        // A callee without keyword parameters (including builtins, variant
        // constructors, and anything else not resolvable to a user function)
        // takes no keyword arguments; leave its (all-positional) call alone.
        let info = match self.fns.get(name) {
            Some(info) if !info.kw.is_empty() => info,
            found => {
                if let Some((k, _, kspan)) = by_kw.first() {
                    // A known function whose parameter `k` exists but is
                    // positional gets the more specific message.
                    if found.is_some_and(|info| info.positional.iter().any(|p| p == k)) {
                        return Err(Error::at(
                            format!(
                                "parameter {k:?} of fn {:?} is positional (it has no default) \
                                 and cannot be passed by keyword",
                                display(name),
                            ),
                            kspan.clone(),
                        ));
                    }
                    return Err(Error::at(
                        format!(
                            "{:?} has no keyword parameter {k:?} (a keyword parameter is one \
                             declared with a default, e.g. `k: i64 = 0`)",
                            display(name)
                        ),
                        kspan.clone(),
                    ));
                }
                return Ok(positional);
            }
        };

        if positional.len() != info.positional.len() {
            return Err(Error::at(
                format!(
                    "fn {:?} expects {} positional arg(s), got {} (its keyword parameter{} must \
                     be passed by keyword: {})",
                    display(name),
                    info.positional.len(),
                    positional.len(),
                    if info.kw.len() == 1 { "" } else { "s" },
                    info.kw
                        .iter()
                        .map(|(k, _)| format!("{k:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                span.clone(),
            ));
        }

        // Match each keyword argument to a keyword parameter by name.
        let kw_names: Vec<&str> = info.kw.iter().map(|(k, _)| k.as_str()).collect();
        let mut supplied: Vec<Option<Expr>> = vec![None; kw_names.len()];
        for (k, v, kspan) in by_kw {
            let Some(i) = kw_names.iter().position(|n| *n == k) else {
                // Naming a *positional* parameter gets its own message: only a
                // keyword parameter may be supplied by keyword.
                if info.positional.iter().any(|p| *p == k) {
                    return Err(Error::at(
                        format!(
                            "parameter {k:?} of fn {:?} is positional (it has no default) and \
                             cannot be passed by keyword",
                            display(name),
                        ),
                        kspan,
                    ));
                }
                return Err(Error::at(
                    format!(
                        "fn {:?} has no keyword parameter {k:?}; its keyword parameter{} {}",
                        display(name),
                        if kw_names.len() == 1 { " is" } else { "s are" },
                        kw_names
                            .iter()
                            .map(|k| format!("{k:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    kspan,
                ));
            };
            if supplied[i].is_some() {
                return Err(Error::at(
                    format!("duplicate keyword argument {k:?}"),
                    kspan,
                ));
            }
            supplied[i] = Some(v);
        }

        // Fill each omitted keyword parameter from its (expanded) default. The
        // spliced expression keeps the default's own span, so an error inside
        // it points at the declaration.
        let defaults = self.expanded_defaults(name)?;
        positional.extend(
            supplied
                .into_iter()
                .zip(defaults)
                .map(|(s, d)| s.unwrap_or(d)),
        );
        Ok(positional)
    }

    /// Structurally expand `e`: rewrite every call as [`expand_call_args`]
    /// describes, tracking `locals` so a call through (or a reference to) a
    /// local binding is never confused with a global function of the same name.
    fn expand_expr(&mut self, e: &Expr, locals: &HashSet<String>) -> Result<Expr, Error> {
        let with = |name: &str| -> HashSet<String> {
            let mut s = locals.clone();
            s.insert(name.to_string());
            s
        };
        let kind = match &e.kind {
            ExprKind::Num(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Unit
            | ExprKind::None => e.kind.clone(),
            // A bare reference to a function with keyword parameters (passing
            // it as a value): there is no call site to fill defaults at, and a
            // function type has no keyword parameters, so reject it.
            ExprKind::Ident(name) => {
                if !locals.contains(name)
                    && self.fns.get(name).is_some_and(|info| !info.kw.is_empty())
                {
                    return Err(Error::at(
                        format!(
                            "fn {:?} has keyword parameters, so it cannot be passed as a \
                             function value",
                            display(name)
                        ),
                        e.span.clone(),
                    ));
                }
                e.kind.clone()
            }
            // A keyword argument outside a call's argument list (the grammar
            // shares the argument list with array literals/patterns).
            ExprKind::KwArg(..) => {
                return Err(Error::at(
                    "a keyword argument is only valid in a function call's arguments".to_string(),
                    e.span.clone(),
                ));
            }
            ExprKind::Call(name, args, method_style) => {
                // Expand within each argument first (a keyword argument's value
                // is expanded; the KwArg wrapper itself is handled below).
                let args: Vec<Expr> = args
                    .iter()
                    .map(|a| match &a.kind {
                        ExprKind::KwArg(k, v) => {
                            let v = self.expand_expr(v, locals)?;
                            Ok(Expr::new(
                                ExprKind::KwArg(k.clone(), Box::new(v)),
                                a.span.clone(),
                            ))
                        }
                        _ => self.expand_expr(a, locals),
                    })
                    .collect::<Result<_, _>>()?;
                if locals.contains(name) {
                    // A call through a function-typed local: function types
                    // have no keyword parameters.
                    if let Some(kw) = args.iter().find(|a| matches!(a.kind, ExprKind::KwArg(..))) {
                        return Err(Error::at(
                            format!(
                                "{name:?} is a function value, and a function value takes no \
                                 keyword arguments"
                            ),
                            kw.span.clone(),
                        ));
                    }
                    ExprKind::Call(name.clone(), args, *method_style)
                } else {
                    ExprKind::Call(
                        name.clone(),
                        self.expand_call_args(name, args, &e.span)?,
                        *method_style,
                    )
                }
            }
            ExprKind::Construct(name, inits) => ExprKind::Construct(
                name.clone(),
                inits
                    .iter()
                    .map(|fi| {
                        Ok(FieldInit {
                            name: fi.name.clone(),
                            value: self.expand_expr(&fi.value, locals)?,
                        })
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            ExprKind::Field(obj, field) => {
                ExprKind::Field(Box::new(self.expand_expr(obj, locals)?), field.clone())
            }
            ExprKind::Match(scrutinee, arms) => {
                let new_arms = arms
                    .iter()
                    .map(|arm| {
                        let mut arm_locals = locals.clone();
                        for b in arm.pattern.bindings() {
                            arm_locals.insert(b.clone());
                        }
                        Ok(MatchArm {
                            pattern: arm.pattern.clone(),
                            body: self.expand_expr(&arm.body, &arm_locals)?,
                            span: arm.span.clone(),
                        })
                    })
                    .collect::<Result<_, Error>>()?;
                ExprKind::Match(Box::new(self.expand_expr(scrutinee, locals)?), new_arms)
            }
            ExprKind::Neg(x) => ExprKind::Neg(Box::new(self.expand_expr(x, locals)?)),
            ExprKind::Not(x) => ExprKind::Not(Box::new(self.expand_expr(x, locals)?)),
            ExprKind::Binop(a, op, b) => ExprKind::Binop(
                Box::new(self.expand_expr(a, locals)?),
                *op,
                Box::new(self.expand_expr(b, locals)?),
            ),
            ExprKind::If(c, t, f) => ExprKind::If(
                Box::new(self.expand_expr(c, locals)?),
                Box::new(self.expand_expr(t, locals)?),
                Box::new(self.expand_expr(f, locals)?),
            ),
            ExprKind::Let(name, value, body) => ExprKind::Let(
                name.clone(),
                Box::new(self.expand_expr(value, locals)?),
                Box::new(self.expand_expr(body, &with(name))?),
            ),
            ExprKind::LetMut(name, value, body) => ExprKind::LetMut(
                name.clone(),
                Box::new(self.expand_expr(value, locals)?),
                Box::new(self.expand_expr(body, &with(name))?),
            ),
            // The LHS is a place rooted at a local mut binding (idents/fields
            // only — no calls), so it needs no expansion.
            ExprKind::Assign(lhs, value, body) => ExprKind::Assign(
                lhs.clone(),
                Box::new(self.expand_expr(value, locals)?),
                Box::new(self.expand_expr(body, locals)?),
            ),
            ExprKind::For(var, iterable, body) => ExprKind::For(
                var.clone(),
                Box::new(self.expand_expr(iterable, locals)?),
                Box::new(self.expand_expr(body, &with(var))?),
            ),
            ExprKind::While(cond, body) => ExprKind::While(
                Box::new(self.expand_expr(cond, locals)?),
                Box::new(self.expand_expr(body, locals)?),
            ),
            ExprKind::ArrayLit(elems) => ExprKind::ArrayLit(self.expand_all(elems, locals)?),
            ExprKind::SetLit(elems) => ExprKind::SetLit(self.expand_all(elems, locals)?),
            ExprKind::TupleLit(elems) => ExprKind::TupleLit(self.expand_all(elems, locals)?),
            ExprKind::DictLit(pairs) => ExprKind::DictLit(
                pairs
                    .iter()
                    .map(|(k, v)| Ok((self.expand_expr(k, locals)?, self.expand_expr(v, locals)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            ExprKind::Index(obj, index) => ExprKind::Index(
                Box::new(self.expand_expr(obj, locals)?),
                Box::new(self.expand_expr(index, locals)?),
            ),
            ExprKind::Slice(obj, start, end) => ExprKind::Slice(
                Box::new(self.expand_expr(obj, locals)?),
                Box::new(self.expand_expr(start, locals)?),
                end.as_ref()
                    .map(|x| Ok::<_, Error>(Box::new(self.expand_expr(x, locals)?)))
                    .transpose()?,
            ),
            ExprKind::Try(x) => ExprKind::Try(Box::new(self.expand_expr(x, locals)?)),
            ExprKind::Seq(a, b) => ExprKind::Seq(
                Box::new(self.expand_expr(a, locals)?),
                Box::new(self.expand_expr(b, locals)?),
            ),
            ExprKind::Return(x) => ExprKind::Return(Box::new(self.expand_expr(x, locals)?)),
            ExprKind::Lambda(params, body) => {
                let mut inner = locals.clone();
                for p in params {
                    inner.insert(p.name.clone());
                }
                ExprKind::Lambda(
                    params.iter().map(LambdaParam::clone).collect(),
                    Box::new(self.expand_expr(body, &inner)?),
                )
            }
        };
        Ok(Expr::new(kind, e.span.clone()))
    }

    fn expand_all(&mut self, elems: &[Expr], locals: &HashSet<String>) -> Result<Vec<Expr>, Error> {
        elems.iter().map(|x| self.expand_expr(x, locals)).collect()
    }
}

/// A function's name as the user wrote it: the loader's cross-file mangling
/// (`__m3__foo` → `foo`) and the builtins' reserved prefix (`__builtin_len` →
/// `len`) both stripped, mirroring the checker's own `display`.
fn display(name: &str) -> &str {
    let name = super::unmangled_name(name);
    name.strip_prefix("__builtin_").unwrap_or(name)
}
