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
//! A default may also read the parameters declared **before** it —
//! `fn f(a: T, b: T = a)`. Splicing happens at the call site, where those
//! parameters don't exist as names, so each read is materialized as a `let`
//! binding of the parameter's name to the argument that call passes for it:
//! `f(x)` becomes `f(x, { let a: T = x; a })`. Ordinary scoping then does the
//! rest — a `let` inside the default shadows the parameter exactly as it did in
//! the declaration, and a caller's own `a` is untouched. An argument a default
//! reads is evaluated **once**: anything but a name or literal is hoisted into
//! its own binding around the call, along with every argument before it so the
//! left-to-right evaluation order the call site had is preserved. Only earlier
//! parameters are in scope, so a forward or self reference is rejected here
//! rather than silently resolving to something in the caller's scope.
//!
//! A **variant case** declares its payload the same way (`Circle(r: i64,
//! color: str = "red")`) and gets the same treatment: its constructor is
//! registered under the loader's variant-qualified `Case@Variant` name, which
//! is exactly what every construction call carries by now, so one set of rules
//! covers `f(1, k = 2)` and `Circle(1, color = "blue")` alike.
//!
//! [`flatten`]: super::Loader::flatten

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{
    Expr, ExprKind, FieldInit, Function, Item, LambdaParam, MatchArm, Param, Program, Signature,
    Type, VariantCase, VariantDecl,
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
        match item {
            Item::Fn(f) => {
                fns.insert(f.name.clone(), FnKwInfo::from_sig(&f.name, &f.sig)?);
            }
            // A variant case's constructor is a callee like any other: by now
            // every construction of it — bare `Circle(..)`, qualified
            // `Shape.Circle(..)`, or reached through an import — has been
            // rewritten to the variant-qualified `Case@Variant`, so that is the
            // name its keyword info is filed under.
            Item::Variant(v) => {
                for c in &v.cases {
                    let name = ctor_name(v, c);
                    let info = FnKwInfo::from_params(&name, &case_params(c))?;
                    fns.insert(name, info);
                }
            }
            _ => {}
        }
    }

    let struct_fields = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Struct(s) => Some((
                s.name.clone(),
                (
                    s.fields.iter().map(|f| f.name.clone()).collect(),
                    s.is_generic(),
                ),
            )),
            _ => None,
        })
        .collect();

    let mut cx = Expander {
        fns,
        defaults: HashMap::new(),
        expanding: Vec::new(),
        spreads: 0,
        struct_fields,
        struct_spreads: 0,
        kw_tmps: 0,
    };

    let items = program
        .items
        .iter()
        .map(|item| {
            // A case's defaults are expanded and stored back for the same
            // reason a keyword parameter's are: the declaration keeps a copy
            // the checker reads, and it must not hold an unexpanded call.
            if let Item::Variant(v) = item {
                let mut cases = v.cases.clone();
                for c in &mut cases {
                    let expanded = cx.expanded_defaults(&ctor_name(v, c))?;
                    for (slot, d) in c
                        .payload
                        .iter_mut()
                        .filter(|slot| slot.default.is_some())
                        .zip(expanded)
                    {
                        slot.default = Some(d);
                    }
                }
                return Ok(Item::Variant(VariantDecl { cases, ..v.clone() }));
            }
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
    /// Every parameter in declaration order, paired with the annotation to put
    /// on the call-site binding that stands in for it when a later default
    /// reads it — the parameter's declared type, so a bare literal argument
    /// flexes to a narrow width exactly as it would in argument position
    /// (`fn f(a: u8, b: u8 = a)`, called `f(5)`). `None` where the type isn't a
    /// runtime type yet — it mentions a type variable or `any`, and which type
    /// that is isn't known until monomorphization, so the binding is left to
    /// infer like the spread desugaring's generic case.
    params: Vec<(String, Option<Type>)>,
}

impl FnKwInfo {
    /// Split `sig`'s parameters into positional and keyword, enforcing the two
    /// declaration rules: once a parameter has a default, every later one must
    /// too (keyword parameters come last), and a default may only read
    /// parameters declared *before* it.
    fn from_sig(name: &str, sig: &Signature) -> Result<FnKwInfo, Error> {
        FnKwInfo::from_params(name, &sig.params)
    }

    /// [`FnKwInfo::from_sig`] over a bare parameter list — what a variant
    /// case's payload becomes (see [`case_params`]).
    fn from_params(name: &str, sig_params: &[Param]) -> Result<FnKwInfo, Error> {
        let mut kw: Vec<(String, Expr)> = Vec::new();
        let mut positional = Vec::new();
        for p in sig_params {
            match &p.default {
                Some(d) => kw.push((p.name.clone(), d.clone())),
                None => {
                    if let Some((kw_name, kw_default)) = kw.last() {
                        return Err(Error::at(
                            format!(
                                "{}: parameter {:?} has no default but follows keyword \
                                 parameter {kw_name:?}; parameters with defaults must come last",
                                describe(name),
                                p.name
                            ),
                            kw_default.span.clone(),
                        ));
                    }
                    positional.push(p.name.clone());
                }
            }
        }
        // A default is filled in at the call site from the arguments that call
        // passes, so it can only read parameters already decided there: the
        // ones declared before it. A reference to itself or to a later
        // parameter has nothing to bind to — and left alone it would resolve to
        // whatever the *caller* happens to have in scope under that name — so
        // it is refused here, where the declaration is what the span points at.
        for (i, p) in sig_params.iter().enumerate() {
            let Some(d) = &p.default else { continue };
            let free = aipl_syntax::free_idents(d);
            let Some(later) = sig_params[i..].iter().find(|q| free.contains(&q.name)) else {
                continue;
            };
            let relation = if later.name == p.name {
                "itself".to_string()
            } else {
                format!("parameter {:?}, which is declared after it", later.name)
            };
            return Err(Error::at(
                format!(
                    "{}: the default for parameter {:?} reads {relation} — a default may \
                     only read parameters declared before it",
                    describe(name),
                    p.name
                ),
                d.span.clone(),
            ));
        }
        let params = sig_params
            .iter()
            .map(|p| {
                let ann = (!aipl_syntax::mentions_typevar(&p.ty)).then(|| p.ty.clone());
                (p.name.clone(), ann)
            })
            .collect();
        Ok(FnKwInfo {
            positional,
            kw,
            params,
        })
    }
}

/// The name a variant case's constructor is called by after loader rewriting:
/// the variant-qualified `Case@Variant`. Shared so the registration and the
/// write-back agree on one spelling.
fn ctor_name(v: &VariantDecl, c: &VariantCase) -> String {
    format!("{}@{}", c.name, v.name)
}

/// A variant case's payload slots as the parameter list its constructor takes,
/// so one set of rules — [`FnKwInfo::from_params`] and everything downstream of
/// it — covers a case and a function alike. An *unnamed* slot (`Circle(i64)`)
/// gets a synthetic name no source identifier can collide with: it exists only
/// so the slots line up positionally, and nothing can read it by name.
fn case_params(c: &VariantCase) -> Vec<Param> {
    c.payload
        .iter()
        .enumerate()
        .map(|(i, slot)| Param {
            name: slot
                .name
                .clone()
                .unwrap_or_else(|| format!("{}{i}", aipl_syntax::CASE_SLOT_PREFIX)),
            ty: slot.ty.clone(),
            mutable: false,
            variadic: false,
            default: slot.default.clone(),
        })
        .collect()
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
    /// Serial number for the synthetic bindings `desugar_spread` introduces, so
    /// nested array literals each get their own accumulator.
    spreads: usize,
    /// Per struct: its field names in declaration order — what a `..base` struct
    /// spread expands to — and whether it is a generic template. Only *names* are
    /// needed for the expansion, so a generic struct contributes its template's
    /// list: every instance of `Box<T>` has the same fields, and which instance
    /// this is isn't known until monomorphization. The generic flag decides
    /// whether the expansion can pin the operand's type (see
    /// `desugar_struct_spread`).
    struct_fields: HashMap<String, (Vec<String>, bool)>,
    /// Serial number for the binding `desugar_struct_spread` introduces when the
    /// spread operand isn't already a plain name.
    struct_spreads: usize,
    /// Serial number for the bindings `expand_call_args` hoists arguments into
    /// when a keyword parameter's default reads them.
    kw_tmps: usize,
}

/// Whether `e` can be written into the call more than once for free: a name or
/// a literal has no side effect and no evaluation cost worth a binding, so a
/// default that reads it can mention it directly. Anything else is hoisted.
fn mentionable(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Ident(_)
            | ExprKind::Num(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Unit
            | ExprKind::None
    )
}

/// Whether this init is a `..base` struct spread rather than a named field.
/// The `Spread` value is what identifies it (the name is empty and carries no
/// meaning) — see the `FieldInit` build action in `aipl-parser`.
fn is_spread(fi: &FieldInit) -> bool {
    matches!(fi.value.kind, ExprKind::Spread(_))
}

impl Expander {
    /// Replace a struct spread — `T { ..base, a: 1 }` — with the fields it
    /// stands for: every field of `T` the literal does not give explicitly is
    /// read off `base`. So with `struct T { a: i64, b: i64, c: i64 }`,
    ///
    /// ```text
    /// T { ..base, a: 1 }  =>  T { a: 1, b: base.b, c: base.c }
    /// ```
    ///
    /// Done here, in the loader, for the same reason the array spread is: it
    /// runs before the checker, so nothing downstream — checker, monomorphizer,
    /// codegen — ever sees a spread, and the rule lives in exactly one place.
    /// Only field *names* are needed, which is why a generic struct can be
    /// expanded from its template (see `struct_fields`).
    ///
    /// **The operand is evaluated once.** A spread of anything but a plain name
    /// is bound to a synthetic `let` first, because the expansion mentions it
    /// once per field — `T { ..f() }` must not call `f` three times.
    ///
    /// Returns the rewritten init list plus the binding to wrap around the
    /// construct — `(name, operand, type annotation)` — or `None` when there was
    /// no spread.
    fn desugar_struct_spread(
        &mut self,
        name: &str,
        inits: &[FieldInit],
        span: &Span,
    ) -> Result<(Vec<FieldInit>, Option<(String, Expr, Option<Type>)>), Error> {
        let spreads = inits.iter().filter(|fi| is_spread(fi)).count();
        if spreads == 0 {
            return Ok((inits.to_vec(), None));
        }
        // One spread, written first. Both restrictions exist so the literal has
        // exactly one reading: two spreads would have to be ordered against each
        // other, and a trailing spread reads as "and the rest from base" while
        // this one means "start from base, then override" — the same text with
        // two plausible meanings. Requiring it first makes the override
        // direction unambiguous at a glance.
        if spreads > 1 {
            return Err(Error::at(
                "a struct literal takes at most one \"..\" spread".to_string(),
                span.clone(),
            ));
        }
        if !is_spread(&inits[0]) {
            return Err(Error::at(
                "a \"..\" spread must come first in a struct literal, before the fields \
                 that override it"
                    .to_string(),
                inits
                    .iter()
                    .find(|fi| is_spread(fi))
                    .map_or_else(|| span.clone(), |fi| fi.value.span.clone()),
            ));
        }
        let ExprKind::Spread(base) = &inits[0].value.kind else {
            unreachable!("is_spread matched")
        };
        let sspan = inits[0].value.span.clone();
        let Some((field_names, generic)) = self.struct_fields.get(name).cloned() else {
            return Err(Error::at(
                format!("cannot spread into {name:?}: no such struct"),
                sspan,
            ));
        };
        let given = &inits[1..];
        // The operand is always bound, for two reasons. It is evaluated *once*,
        // however many fields it stands for — the expansion mentions it per
        // field, so `T { ..f() }` must not call `f` three times. And the binding
        // carries the type annotation that makes the spread same-type: a spread
        // copies fields from another value of *this* struct, so `FmtError { ..e }`
        // where `e` is a `LexError` is rejected even though the fields line up.
        // Two structs that merely coincide in shape are not interchangeable, and
        // a spread that crossed between them would silently change meaning the
        // moment either one gained or renamed a field.
        //
        // A generic target can't be annotated — `Box` names a template, not a
        // type, and its instance isn't known until monomorphization — so those
        // are bound without one. Their field *types* are still checked against
        // the instance, which catches a mismatched instance (`Box<str>` into
        // `Box<i64>`); what it doesn't catch is a different struct that happens
        // to share the field names.
        let k = self.struct_spreads;
        self.struct_spreads += 1;
        let tmp = format!("{}{k}", aipl_syntax::SPREAD_BASE_PREFIX);
        let base_ref = Expr::new(ExprKind::Ident(tmp.clone()), sspan.clone());
        let ann = if generic {
            None
        } else {
            Some(Type::Named(name.to_string()))
        };
        let binding = Some((tmp, (**base).clone(), ann));
        // Definition order, explicit fields winning over the spread. A field the
        // struct doesn't declare is left in place for the checker to reject, so
        // the diagnostic for a typo'd field name is the usual one.
        let mut out: Vec<FieldInit> = Vec::with_capacity(field_names.len());
        for fname in &field_names {
            match given.iter().find(|fi| &fi.name == fname) {
                Some(fi) => out.push(fi.clone()),
                None => out.push(FieldInit {
                    name: fname.clone(),
                    value: Expr::new(
                        ExprKind::Field(Box::new(base_ref.clone()), fname.clone()),
                        sspan.clone(),
                    ),
                }),
            }
        }
        for fi in given {
            if !field_names.contains(&fi.name) {
                out.push(fi.clone());
            }
        }
        Ok((out, binding))
    }

    /// Rewrite an array literal containing one or more `..xs` elements into a
    /// block that builds the array in one pre-sized allocation:
    ///
    /// ```text
    /// [..a, b, ..c, d]
    ///   =>  mut $s = __aipl_arr_reserve(a, 2 + len(c));
    ///       set $s = __aipl_arr_append($s, b);
    ///       set $s = __aipl_arr_concat($s, c);
    ///       set $s = __aipl_arr_append($s, d);
    ///       $s
    /// ```
    ///
    /// The literal's final length is known here — a constant for the plain
    /// elements plus a `len` call per spread — so `reserve` sizes the buffer
    /// exactly once instead of growing it. `reserve` also hands back a
    /// *uniquely owned* block (reusing the seed's allocation when its refcount
    /// is 1, else copying into a right-sized one), which is what lets the
    /// appends after it write in place without consulting the static
    /// exclusivity analysis — that analysis does not recognize this shape, so
    /// a push-based build copied the whole accumulator per element.
    ///
    /// The seed is the literal's leading run: the first spread's operand when
    /// the literal opens with `..`, otherwise an array literal of the plain
    /// elements before the first spread. Seeding from `[]` instead would leave
    /// the element type as the untyped-empty `__none__`, which codegen refuses
    /// to grow with a `char` (see `arrays/push/err_push_char_from_empty`).
    ///
    /// The intrinsics are emitted under canonical `__builtin_*` names, after
    /// import resolution — so spread syntax needs no `import { push }`. They
    /// are also emitted unconditionally: this pass runs before type checking,
    /// so it cannot know the element representation. Codegen does, and routes
    /// `char[]`/`bool[]` back to the ordinary push lowering.
    fn desugar_spread(&mut self, elems: Vec<Expr>, span: Span) -> ExprKind {
        let k = self.spreads;
        self.spreads += 1;
        let acc = format!("__spread${k}");
        let node = |kind| Expr::new(kind, span.clone());
        let acc_ref = || node(ExprKind::Ident(acc.clone()));

        // The seed, and the elements still to be appended after it.
        let (seed, rest) = match &elems[0].kind {
            ExprKind::Spread(inner) => ((**inner).clone(), &elems[1..]),
            _ => {
                let n = elems
                    .iter()
                    .position(|x| matches!(x.kind, ExprKind::Spread(_)))
                    .unwrap_or(elems.len());
                (node(ExprKind::ArrayLit(elems[..n].to_vec())), &elems[n..])
            }
        };

        // How much `rest` adds: one per plain element, `len(operand)` per
        // spread, summed left to right onto the constant.
        let plain = rest
            .iter()
            .filter(|x| !matches!(x.kind, ExprKind::Spread(_)))
            .count();
        // A bare literal, not a `u64(..)` conversion (that form is gone): this
        // desugaring only runs for a literal that *has* a spread, so the count is
        // always summed with at least one `len()` below and flexes to its `u64`.
        let mut extra = node(ExprKind::Num(plain as i64));
        for elem in rest {
            let ExprKind::Spread(inner) = &elem.kind else {
                continue;
            };
            let len = Expr::new(
                ExprKind::Call("__builtin_len".to_string(), vec![(**inner).clone()], true),
                elem.span.clone(),
            );
            extra = Expr::new(
                ExprKind::Call(
                    "__builtin_wrapping_add".to_string(),
                    vec![extra, len],
                    false,
                ),
                elem.span.clone(),
            );
        }

        // `set <acc> = <intrinsic>(<acc>, <arg>);` then `rest`.
        let step = |f: &str, arg: Expr, rest: Expr, span: &Span| {
            let call = Expr::new(
                ExprKind::Call(f.to_string(), vec![acc_ref(), arg], false),
                span.clone(),
            );
            Expr::new(
                ExprKind::Assign(Box::new(acc_ref()), Box::new(call), Box::new(rest)),
                span.clone(),
            )
        };

        // Fold the trailing elements in from the right onto the block's value.
        let body = rest.iter().rev().fold(acc_ref(), |rest, elem| {
            let espan = &elem.span;
            match &elem.kind {
                ExprKind::Spread(inner) => {
                    step("__aipl_arr_concat", (**inner).clone(), rest, espan)
                }
                _ => step("__aipl_arr_append", elem.clone(), rest, espan),
            }
        });
        let reserved = Expr::new(
            ExprKind::Call("__aipl_arr_reserve".to_string(), vec![seed, extra], false),
            span.clone(),
        );
        ExprKind::LetMut(acc, None, Box::new(reserved), Box::new(body))
    }

    /// The type annotation to keep on a `let T { .. } = value;` scrutinee
    /// binding. `T` pins the pattern to one struct, which is the whole point of
    /// writing it — but a **generic** struct names a template rather than a type,
    /// and there is nothing to pin it to until monomorphization, so the
    /// annotation is dropped for those. The parser cannot make this call: it has
    /// no struct table, and only knows the name the user wrote.
    ///
    /// Every other binding keeps its annotation untouched, so a hand-written
    /// `let b: Box = ..` is still the error it always was.
    fn destructure_ann(&self, name: &str, ty: &Option<Type>) -> Option<Type> {
        if !name.starts_with(aipl_syntax::DESTRUCTURE_BASE_PREFIX) {
            return ty.clone();
        }
        match ty {
            Some(Type::Named(n)) if self.struct_fields.get(n).is_some_and(|(_, g)| *g) => None,
            other => other.clone(),
        }
    }

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
        // The parameters declared before a default are its locals, exactly as
        // the whole parameter list is the body's: a name that is one refers to
        // the parameter, so a call through a function-typed parameter never
        // picks up the keyword machinery of a global of the same name.
        let (defaults, param_names): (Vec<Expr>, Vec<String>) = self
            .fns
            .get(name)
            .map(|info| {
                (
                    info.kw.iter().map(|(_, d)| d.clone()).collect(),
                    info.params.iter().map(|(n, _)| n.clone()).collect(),
                )
            })
            .unwrap_or_default();
        let npos = param_names.len() - defaults.len();
        let expanded: Result<Vec<Expr>, Error> = defaults
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let locals: HashSet<String> = param_names[..npos + i].iter().cloned().collect();
                self.expand_expr(d, &locals)
            })
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
    ///
    /// Returns the positional arguments plus the bindings to wrap around the
    /// call, outermost first — see [`Expander::fill_defaults`]. They are empty
    /// unless a spliced default reads a parameter.
    fn expand_call_args(
        &mut self,
        name: &str,
        args: Vec<Expr>,
        span: &Span,
    ) -> Result<(Vec<Expr>, Vec<(String, Option<Type>, Expr)>), Error> {
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
                                "parameter {k:?} of {} is positional (it has no default) \
                                 and cannot be passed by keyword",
                                describe(name),
                            ),
                            kspan.clone(),
                        ));
                    }
                    return Err(Error::at(
                        format!(
                            "{} has no keyword parameter {k:?} (a keyword parameter is one \
                             declared with a default, e.g. `k: i64 = 0`)",
                            describe(name)
                        ),
                        kspan.clone(),
                    ));
                }
                return Ok((positional, Vec::new()));
            }
        };

        if positional.len() != info.positional.len() {
            return Err(Error::at(
                format!(
                    "{} expects {} positional arg(s), got {} (its keyword parameter{} must \
                     be passed by keyword: {})",
                    describe(name),
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
                            "parameter {k:?} of {} is positional (it has no default) and \
                             cannot be passed by keyword",
                            describe(name),
                        ),
                        kspan,
                    ));
                }
                return Err(Error::at(
                    format!(
                        "{} has no keyword parameter {k:?}; its keyword parameter{} {}",
                        describe(name),
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

        let params = info.params.clone();
        let defaults = self.expanded_defaults(name)?;
        Ok(self.fill_defaults(&params, positional, supplied, defaults, span))
    }

    /// Build the final positional argument list: each supplied argument in
    /// declaration order, with every omitted keyword parameter filled from its
    /// (expanded) default. The spliced expression keeps the default's own span,
    /// so an error inside it points at the declaration.
    ///
    /// A default may read the parameters declared before it. Those names don't
    /// exist at the call site, so each read becomes a `let` of the parameter's
    /// name around the default — `f(x)` with `fn f(a: T, b: T = a)` splices
    /// `{ let a: T = x; a }`. Scoping then behaves as it did in the
    /// declaration: a binder inside the default shadows the parameter, and the
    /// caller's own `a`, if it has one, is untouched.
    ///
    /// An argument that gets read is mentioned more than once, so it is
    /// evaluated **once**, into a binding wrapped around the call — as is every
    /// argument before it, since hoisting only some of them would reorder the
    /// side effects between them. A name or a literal needs no binding: it can
    /// be mentioned again for free.
    ///
    /// Returns the arguments and those bindings, outermost first.
    fn fill_defaults(
        &mut self,
        params: &[(String, Option<Type>)],
        positional: Vec<Expr>,
        supplied: Vec<Option<Expr>>,
        defaults: Vec<Expr>,
        span: &Span,
    ) -> (Vec<Expr>, Vec<(String, Option<Type>, Expr)>) {
        // Which parameters do the defaults actually spliced here read? A
        // supplied keyword argument replaces its default outright, so that
        // default's reads never happen and cost nothing.
        let mut reads: HashSet<String> = HashSet::new();
        for (s, d) in supplied.iter().zip(&defaults) {
            if s.is_none() {
                reads.extend(aipl_syntax::free_idents(d));
            }
        }
        let last_read = params
            .iter()
            .rposition(|(n, _)| reads.contains(n))
            .unwrap_or(0);
        let no_reads = reads.is_empty();

        let npos = positional.len();
        let mut bindings: Vec<(String, Option<Type>, Expr)> = Vec::new();
        // Parameter name -> the expression that reads its argument here.
        let mut arg_of: HashMap<&str, Expr> = HashMap::new();
        let mut out: Vec<Expr> = Vec::with_capacity(params.len());
        for (i, (pname, pty)) in params.iter().enumerate() {
            let arg = match positional.get(i) {
                Some(a) => a.clone(),
                None => match &supplied[i - npos] {
                    Some(v) => v.clone(),
                    None => {
                        let d = &defaults[i - npos];
                        let free = aipl_syntax::free_idents(d);
                        params[..i].iter().rev().fold(d.clone(), |body, (n, ty)| {
                            let Some(a) = arg_of.get(n.as_str()).filter(|_| free.contains(n))
                            else {
                                return body;
                            };
                            let bspan = body.span.clone();
                            Expr::new(
                                ExprKind::Let(
                                    n.clone(),
                                    ty.clone(),
                                    Box::new(a.clone()),
                                    Box::new(body),
                                ),
                                bspan,
                            )
                        })
                    }
                },
            };
            let arg = if no_reads || i > last_read || mentionable(&arg) {
                arg
            } else {
                let tmp = format!("{}{}", aipl_syntax::KWARG_ARG_PREFIX, self.kw_tmps);
                self.kw_tmps += 1;
                bindings.push((tmp.clone(), pty.clone(), arg));
                Expr::new(ExprKind::Ident(tmp), span.clone())
            };
            if reads.contains(pname) {
                arg_of.insert(pname, arg.clone());
            }
            out.push(arg);
        }
        (out, bindings)
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
            // A shim binds functions by name (no call site to fill defaults at);
            // only its body can contain calls.
            ExprKind::Shim(effect, bindings, body) => ExprKind::Shim(
                effect.clone(),
                bindings.clone(),
                Box::new(self.expand_expr(body, locals)?),
            ),
            // A bare reference to a function with keyword parameters (passing
            // it as a value): there is no call site to fill defaults at, and a
            // function type has no keyword parameters, so reject it.
            ExprKind::Ident(name) => {
                if !locals.contains(name)
                    && self.fns.get(name).is_some_and(|info| !info.kw.is_empty())
                {
                    return Err(Error::at(
                        format!(
                            "{} has keyword parameters, so it cannot be passed as a \
                             function value",
                            describe(name)
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
                    let (args, bindings) = self.expand_call_args(name, args, &e.span)?;
                    let call = ExprKind::Call(name.clone(), args, *method_style);
                    // An argument a spliced default reads is evaluated once,
                    // into a binding wrapped around the call.
                    bindings
                        .into_iter()
                        .rev()
                        .fold(Expr::new(call, e.span.clone()), |body, (n, ty, v)| {
                            Expr::new(
                                ExprKind::Let(n, ty, Box::new(v), Box::new(body)),
                                e.span.clone(),
                            )
                        })
                        .kind
                }
            }
            ExprKind::Construct(name, inits) => {
                // Expand any `..base` spread into the fields it stands for
                // *before* recursing, so the reads it introduces are expanded
                // like any other field value.
                let (inits, binding) = self.desugar_struct_spread(name, inits, &e.span)?;
                let built = ExprKind::Construct(
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
                );
                // A spread of a non-name operand evaluates it once, into a
                // binding wrapped around the construct.
                match binding {
                    None => built,
                    Some((tmp, base, ann)) => ExprKind::Let(
                        tmp,
                        ann,
                        Box::new(self.expand_expr(&base, locals)?),
                        Box::new(Expr::new(built, e.span.clone())),
                    ),
                }
            }
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
            // Same scoping as `Match`'s arms: the pattern's bindings are locals
            // for `arm.body` only, not `scrutinee` or `else_b`.
            ExprKind::IfLet(arm, scrutinee, else_b) => {
                let mut then_locals = locals.clone();
                for b in arm.pattern.bindings() {
                    then_locals.insert(b.clone());
                }
                let new_arm = MatchArm {
                    pattern: arm.pattern.clone(),
                    body: self.expand_expr(&arm.body, &then_locals)?,
                    span: arm.span.clone(),
                };
                ExprKind::IfLet(
                    Box::new(new_arm),
                    Box::new(self.expand_expr(scrutinee, locals)?),
                    Box::new(self.expand_expr(else_b, locals)?),
                )
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
            ExprKind::Let(name, ty, value, body) => ExprKind::Let(
                name.clone(),
                self.destructure_ann(name, ty),
                Box::new(self.expand_expr(value, locals)?),
                Box::new(self.expand_expr(body, &with(name))?),
            ),
            ExprKind::LetMut(name, ty, value, body) => ExprKind::LetMut(
                name.clone(),
                ty.clone(),
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
            // A literal with no `..` element keeps its exact shape (and so its
            // exact codegen); one *with* a spread becomes a seed-and-append
            // block — see `desugar_spread`.
            ExprKind::ArrayLit(elems) => {
                let expanded = self.expand_all(elems, locals)?;
                if expanded
                    .iter()
                    .any(|x| matches!(x.kind, ExprKind::Spread(_)))
                {
                    self.desugar_spread(expanded, e.span.clone())
                } else {
                    ExprKind::ArrayLit(expanded)
                }
            }
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
            ExprKind::Spread(x) => ExprKind::Spread(Box::new(self.expand_expr(x, locals)?)),
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
        // `rebuilt`, not `new`: expanding keyword arguments rewrites the kind
        // in place and must keep the spans recorded on it.
        Ok(Expr::rebuilt(kind, e))
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

/// How `name` is named in a diagnostic. A variant case's constructor carries
/// the loader's `Case@Variant` form (only constructors have an `@`), and is
/// described as the case it is rather than as a `fn` the source never declared.
fn describe(name: &str) -> String {
    match name.split_once('@') {
        Some((case, _)) => format!("variant case {case:?}"),
        None => format!("fn {:?}", display(name)),
    }
}
