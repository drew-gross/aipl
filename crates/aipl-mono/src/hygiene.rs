//! Whether a program's names still mean what they meant when they were written.
//!
//! A name is a spelling, and what it refers to is decided by where it stands: a
//! local if a binder of that spelling encloses it, otherwise a global. Any pass
//! that moves code into another scope can change that decision without touching
//! the name — a callee's call to the global `reads`, put inside a caller that has
//! a parameter `reads`, now names the parameter — and nothing downstream can tell,
//! because the program is still well-formed. It fails later, in code that has no
//! idea which pass was to blame.
//!
//! This reads a program the way the checker would, by scope, and reports what a
//! reader of the *spellings* could be misled by. [`optimize`](crate::passes::optimize)
//! runs it after every pass when `AIPL_HYGIENE=report` is set, and prints what the
//! pass introduced, which is the one place the culprit is still known.
//!
//! What it reports ([`Kind`]):
//!
//! - **Dangling** — an identifier that names no binder in scope and no function,
//!   a call through a local ([`Callee::Local`]) whose binder is not in scope, or
//!   a call of a global ([`Callee::User`]) that is not a function (unless it is
//!   written as a method, which may call a function held in a field). A pass that
//!   renames a binder and one of its uses apart leaves this. A call's target
//!   says which kind of name it is, so a local can no more capture a global's
//!   call than the reverse.
//! - **Rebound synthetic** — a compiler-generated name (one with a `$`, which
//!   users cannot write) bound twice in one function. Each is minted to be unique,
//!   so a second binder is a counter or a derivation that reused a value.
//! - **Shadows** — a binder whose spelling an enclosing binder already has. The
//!   source can shadow on purpose; one a pass introduces is a binder it put under
//!   a caller's variable without renaming it, and it is harmless exactly until the
//!   copied code mentions a global of that spelling.
//!
//! Violations are keyed by function, kind and name, so a pass is blamed only for
//! the ones that were not there before it ran.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

use aipl_syntax::ast::{Callee, Expr, ExprKind};

use crate::passes::Optimizable;

/// What a [`Violation`] is about. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Dangling,
    ReboundSynthetic,
    Shadows,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Dangling => "dangling",
            Kind::ReboundSynthetic => "rebound-synthetic",
            Kind::Shadows => "shadows",
        })
    }
}

/// One place a program's names could mislead.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Violation {
    pub function: String,
    pub kind: Kind,
    pub name: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?} in {}", self.kind, self.name, self.function)
    }
}

/// Whether the check is on: `AIPL_HYGIENE=report` prints what each pass
/// introduced, and `AIPL_HYGIENE=strict` also panics on the first. Unset, it
/// costs nothing.
pub fn enabled() -> bool {
    mode().is_some()
}

/// Whether a violation is a panic rather than a line on stderr.
pub fn strict() -> bool {
    mode().as_deref() == Some("strict")
}

fn mode() -> Option<String> {
    std::env::var("AIPL_HYGIENE")
        .ok()
        .filter(|v| v == "report" || v == "strict")
}

/// Every violation in `program`.
pub fn violations(program: &impl Optimizable) -> BTreeSet<Violation> {
    let views = program.functions();
    let globals: HashSet<&str> = views.iter().map(|f| f.name).collect();
    let mut out = BTreeSet::new();
    for f in &views {
        for body in std::iter::once(f.body).chain(f.test_body) {
            let mut walk = Walk {
                function: f.name,
                globals: &globals,
                scope: f.params.clone(),
                binders: HashMap::new(),
                out: &mut out,
            };
            for p in &f.params {
                *walk.binders.entry(p.clone()).or_default() += 1;
            }
            walk.expr(body);
        }
    }
    out
}

/// What a reference may resolve to.
#[derive(Clone, Copy)]
enum Target {
    /// A call through a local: a binder in scope.
    Local,
    /// A call of a global: a function.
    Global,
    /// An identifier, which is a local or a function used as a value.
    Either,
}

struct Walk<'a> {
    function: &'a str,
    globals: &'a HashSet<&'a str>,
    /// The locals in scope, innermost last.
    scope: Vec<String>,
    /// How many binders of each spelling the function has had so far.
    binders: HashMap<String, usize>,
    out: &'a mut BTreeSet<Violation>,
}

impl Walk<'_> {
    fn report(&mut self, kind: Kind, name: &str) {
        self.out.insert(Violation {
            function: self.function.to_string(),
            kind,
            name: name.to_string(),
        });
    }

    /// Open a binder of `name` over what follows, until [`Walk::unbind`].
    fn bind(&mut self, name: &str) {
        // `_` names a slot a pattern leaves alone; it binds nothing to shadow.
        if name != "_" && self.scope.iter().any(|n| n == name) {
            self.report(Kind::Shadows, name);
        }
        let times = self.binders.entry(name.to_string()).or_default();
        *times += 1;
        if *times > 1 && name.contains('$') {
            self.report(Kind::ReboundSynthetic, name);
        }
        self.scope.push(name.to_string());
    }

    fn unbind(&mut self, count: usize) {
        self.scope.truncate(self.scope.len() - count);
    }

    /// A use of `name`, which `target` says is a local, a global, or a bare
    /// identifier that is either.
    fn reference(&mut self, name: &str, target: Target) {
        let local = self.scope.iter().any(|n| n == name);
        // A builtin is not an item of the program, and is always in reach.
        let global = self.globals.contains(name) || name.starts_with("__builtin_");
        let resolves = match target {
            Target::Local => local,
            Target::Global => global,
            Target::Either => local || global,
        };
        // A case's constructor is spelled `Case@Variant`, and neither a binder
        // nor a function.
        if !resolves && !name.contains('@') {
            self.report(Kind::Dangling, name);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Ident(n) => self.reference(n, Target::Either),
            ExprKind::Call(callee, args, method_style) => {
                match callee {
                    Callee::Local(n) => self.reference(n, Target::Local),
                    // `s.f(x)` calls the function held in `s`'s field `f` when
                    // `s` has one, and no global names it.
                    Callee::User(n) if !*method_style => self.reference(n, Target::Global),
                    _ => {}
                }
                for a in args {
                    self.expr(a);
                }
            }
            ExprKind::Let(n, _, value, body) | ExprKind::LetMut(n, _, value, body) => {
                self.expr(value);
                self.bind(n);
                self.expr(body);
                self.unbind(1);
            }
            ExprKind::For(n, index, iterable, body) => {
                self.expr(iterable);
                self.bind(n);
                let mut bound = 1;
                if let Some(index) = index {
                    self.bind(index);
                    bound += 1;
                }
                self.expr(body);
                self.unbind(bound);
            }
            ExprKind::Lambda(params, body) => {
                for p in params {
                    self.bind(&p.name);
                }
                self.expr(body);
                self.unbind(params.len());
            }
            ExprKind::Match(scrutinee, arms) => {
                self.expr(scrutinee);
                for arm in arms {
                    let names = arm.pattern.bindings();
                    for n in &names {
                        self.bind(n);
                    }
                    self.expr(&arm.body);
                    self.unbind(names.len());
                }
            }
            ExprKind::IfLet(arm, scrutinee, otherwise) => {
                self.expr(scrutinee);
                let names = arm.pattern.bindings();
                for n in &names {
                    self.bind(n);
                }
                self.expr(&arm.body);
                self.unbind(names.len());
                self.expr(otherwise);
            }
            // The place being assigned is a use of its root, which `children`
            // leaves out.
            ExprKind::Assign(place, value, rest) => {
                self.expr(place);
                self.expr(value);
                self.expr(rest);
            }
            _ => {
                for c in crate::children(e) {
                    self.expr(c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use aipl_syntax::ast::{Function, Item, Program, Signature};
    use aipl_syntax::SpanStr;

    use super::*;

    fn expr(kind: ExprKind) -> Expr {
        Expr::new(kind, 0..0)
    }

    fn ident(name: &str) -> Expr {
        expr(ExprKind::Ident(name.to_string()))
    }

    fn number() -> Expr {
        expr(ExprKind::Num(1))
    }

    fn call(name: &str) -> Expr {
        expr(ExprKind::Call(
            Callee::User(name.to_string()),
            Vec::new(),
            false,
        ))
    }

    fn bind(name: &str, rest: Expr) -> Expr {
        expr(ExprKind::Let(
            name.to_string(),
            None,
            Box::new(number()),
            Box::new(rest),
        ))
    }

    fn function(name: &str, body: Expr) -> Item {
        Item::Fn(Function {
            name: SpanStr::synthetic(name.to_string()),
            is_pub: false,
            sig: Signature {
                type_vars: Vec::new(),
                params: Vec::new(),
                effects: Vec::new(),
                return_ty: None,
            },
            body,
            test_body: None,
            test_fns: Vec::new(),
            doc: None,
        })
    }

    /// What `violations` finds in a program of `fns`, as `(function, kind, name)`.
    fn found(fns: Vec<Item>) -> Vec<(String, String, String)> {
        let program = Program {
            items: fns,
            sources: Vec::new(),
            doc: None,
        };
        violations(&program)
            .into_iter()
            .map(|v| (v.function, v.kind.to_string(), v.name))
            .collect()
    }

    fn says(function: &str, kind: &str, name: &str) -> (String, String, String) {
        (function.to_string(), kind.to_string(), name.to_string())
    }

    #[test]
    fn a_reference_to_a_binder_in_scope_or_a_function_is_clean() {
        assert_eq!(
            found(vec![
                function("g", number()),
                function(
                    "f",
                    bind(
                        "x",
                        expr(ExprKind::Seq(Box::new(ident("x")), Box::new(call("g"))))
                    )
                )
            ]),
            vec![]
        );
    }

    #[test]
    fn a_name_nothing_binds_dangles() {
        assert_eq!(
            found(vec![function("f", bind("x", ident("$inl2_x")))]),
            vec![says("f", "dangling", "$inl2_x")]
        );
        // A binder's scope is its body: the use after it is out of reach.
        assert_eq!(
            found(vec![function(
                "f",
                expr(ExprKind::Seq(
                    Box::new(bind("x", number())),
                    Box::new(ident("x"))
                ))
            )]),
            vec![says("f", "dangling", "x")]
        );
    }

    #[test]
    fn a_call_of_a_global_is_not_captured_by_a_binder_of_its_spelling() {
        assert_eq!(
            found(vec![
                function("reads", number()),
                function("f", bind("reads", call("reads")))
            ]),
            vec![]
        );
    }

    #[test]
    fn a_method_call_may_call_a_function_held_in_a_field() {
        let method = |name: &str| {
            expr(ExprKind::Call(
                Callee::User(name.to_string()),
                Vec::new(),
                true,
            ))
        };
        assert_eq!(found(vec![function("f", method("step"))]), vec![]);
        assert_eq!(
            found(vec![function("f", call("step"))]),
            vec![says("f", "dangling", "step")]
        );
    }

    #[test]
    fn a_builtin_is_always_in_reach() {
        assert_eq!(
            found(vec![function("f", ident("__builtin_is_digit"))]),
            vec![]
        );
    }

    #[test]
    fn a_call_through_a_local_needs_the_local_in_scope() {
        let through = |name: &str| {
            expr(ExprKind::Call(
                Callee::Local(name.to_string()),
                Vec::new(),
                false,
            ))
        };
        assert_eq!(found(vec![function("f", bind("g", through("g")))]), vec![]);
        assert_eq!(
            found(vec![function("g", number()), function("f", through("g"))]),
            vec![says("f", "dangling", "g")]
        );
    }

    #[test]
    fn a_binder_under_one_of_its_own_spelling_shadows() {
        assert_eq!(
            found(vec![function("f", bind("x", bind("x", ident("x"))))]),
            vec![says("f", "shadows", "x")]
        );
    }

    #[test]
    fn a_generated_name_bound_twice_in_one_function_is_rebound() {
        // Disjoint scopes: neither shadows the other, but the name was minted to
        // be unique and is not.
        let twice = expr(ExprKind::Seq(
            Box::new(bind("$inl4_self", number())),
            Box::new(bind("$inl4_self", number())),
        ));
        assert_eq!(
            found(vec![function("f", twice)]),
            vec![says("f", "rebound-synthetic", "$inl4_self")]
        );
    }
}
