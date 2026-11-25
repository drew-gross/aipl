//! Renaming the locals of an expression.
//!
//! A name is a spelling, and moving code under other binders can change what a
//! spelling means. This is how a pass puts code somewhere new without that
//! happening: [`rename_params`] retargets references to chosen names, and
//! [`distinct_binders`] gives every binder the code introduces a name nothing else in the
//! function has (see [`Names`]), so the copy can neither shadow a binder around it nor be
//! shadowed by one.

use std::collections::HashMap;

use crate::ast::{Callee, Expr, ExprKind, FieldInit, LambdaParam, MatchArm, WrittenNames};
use crate::names::Names;
use crate::SpanStr;

/// `e` with every binder it introduces renamed, for a copy of code that already
/// has another copy in the same function. Names it only mentions are left alone.
pub fn distinct_binders(e: &Expr, names: &mut Names) -> Expr {
    Renamer { names: Some(names) }.go(e, &HashMap::new())
}

/// Rebuild `e` (a function body) substituting parameter references per `map`
/// (original → fresh name), respecting shadowing: a `let`/`for`/`match`/lambda
/// binder of a mapped name removes it from the map within that scope (there the
/// name is the local, not the parameter). Body-internal binders are left as-is —
/// only the substituted *parameter* references are renamed.
pub fn rename_params(e: &Expr, map: &HashMap<String, String>) -> Expr {
    Renamer { names: None }.go(e, map)
}

/// What renames: the parameters `map` names and, when it holds a supply, every binder the body
/// introduces.
struct Renamer<'n> {
    names: Option<&'n mut Names>,
}

impl Renamer<'_> {
    /// The map inside a scope that binds `names`: they are the locals there, not the
    /// parameters, and given a supply each is known by a new name, returned alongside.
    fn scope(
        &mut self,
        map: &HashMap<String, String>,
        names: &[String],
    ) -> (HashMap<String, String>, HashMap<String, String>) {
        let mut inner = map.clone();
        let mut renamed = HashMap::new();
        for n in names {
            inner.remove(n);
            if let (Some(supply), true) = (self.names.as_deref_mut(), n != "_") {
                let nm = supply.fresh_binder("inl", n);
                inner.insert(n.clone(), nm.clone());
                renamed.insert(n.clone(), nm);
            }
        }
        (inner, renamed)
    }

    /// `e` with the parameters `map` names renamed. Each `let`, loop variable, pattern binder
    /// and lambda parameter becomes a new `$inl<N>_<name>` when there is a supply, so a body copied
    /// under a caller's variables can neither shadow one nor be shadowed, and two copies of one
    /// body share no binder.
    fn go(&mut self, e: &Expr, map: &HashMap<String, String>) -> Expr {
        let sub = |n: &String| map.get(n).cloned().unwrap_or_else(|| n.clone());
        let known_as = |renamed: &HashMap<String, String>, n: &String| {
            renamed.get(n).cloned().unwrap_or_else(|| n.clone())
        };
        let kind = match &e.kind {
            ExprKind::KwArg(k, x, forwarded) => {
                ExprKind::KwArg(k.clone(), Box::new(self.go(x, map)), *forwarded)
            }
            ExprKind::TypeArgs(x, tys) => {
                ExprKind::TypeArgs(Box::new(self.go(x, map)), tys.clone())
            }
            ExprKind::Spread(x) => ExprKind::Spread(Box::new(self.go(x, map))),
            // A shim binds a function by name: a global, which renaming never touches,
            // or a closure `let` the parser put just outside it, which it does.
            ExprKind::Shim(effect, bindings, body) => ExprKind::Shim(
                effect.clone(),
                bindings
                    .iter()
                    .map(|(op, f)| (op.clone(), sub(f)))
                    .collect(),
                Box::new(self.go(body, map)),
            ),
            ExprKind::Ident(n) => ExprKind::Ident(sub(n)),
            // Only a name can be a renamed parameter: a call through one is a
            // `User` callee, and every other arm names a function the compiler
            // knows, which no parameter can shadow.
            ExprKind::Call(name, args, m) => ExprKind::Call(
                match name {
                    Callee::Local(n) => Callee::Local(sub(n)),
                    known => known.clone(),
                },
                args.iter().map(|a| self.go(a, map)).collect(),
                *m,
            ),
            ExprKind::Num(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::None
            | ExprKind::Unit => e.kind.clone(),
            ExprKind::Neg(x) => ExprKind::Neg(Box::new(self.go(x, map))),
            ExprKind::Try(x) => ExprKind::Try(Box::new(self.go(x, map))),
            ExprKind::Return(x) => ExprKind::Return(Box::new(self.go(x, map))),
            ExprKind::Field(x, fld) => ExprKind::Field(Box::new(self.go(x, map)), fld.clone()),
            ExprKind::Seq(a, b) => {
                ExprKind::Seq(Box::new(self.go(a, map)), Box::new(self.go(b, map)))
            }
            ExprKind::Index(a, b) => {
                ExprKind::Index(Box::new(self.go(a, map)), Box::new(self.go(b, map)))
            }
            ExprKind::While(a, b) => {
                ExprKind::While(Box::new(self.go(a, map)), Box::new(self.go(b, map)))
            }
            ExprKind::If(a, b, c) => ExprKind::If(
                Box::new(self.go(a, map)),
                Box::new(self.go(b, map)),
                Box::new(self.go(c, map)),
            ),
            ExprKind::Slice(a, b, c) => ExprKind::Slice(
                Box::new(self.go(a, map)),
                Box::new(self.go(b, map)),
                c.as_ref().map(|c| Box::new(self.go(c, map))),
            ),
            ExprKind::ArrayLit(xs) => {
                ExprKind::ArrayLit(xs.iter().map(|x| self.go(x, map)).collect())
            }
            ExprKind::SetLit(xs, o) => {
                ExprKind::SetLit(xs.iter().map(|x| self.go(x, map)).collect(), *o)
            }
            ExprKind::TupleLit(xs) => {
                ExprKind::TupleLit(xs.iter().map(|x| self.go(x, map)).collect())
            }
            ExprKind::DictLit(pairs) => ExprKind::DictLit(
                pairs
                    .iter()
                    .map(|(k, v)| (self.go(k, map), self.go(v, map)))
                    .collect(),
            ),
            ExprKind::Construct(name, inits) => ExprKind::Construct(
                name.clone(),
                inits
                    .iter()
                    .map(|i| FieldInit {
                        name: i.name.clone(),
                        value: self.go(&i.value, map),
                    })
                    .collect(),
            ),
            // The LHS is a place expression — a bare ident, or a field chain rooted
            // at one. It used to be left alone on the grounds that it is rooted at a
            // `mut` local and parameters are immutable, but a mutating method's
            // `mut self` *is* a parameter: its body is a run of `set self... `, and
            // leaving those unrenamed would assign to a binding the inlined
            // expression never introduced. Renaming it is right either way — a
            // caller-side local is simply absent from the map.
            ExprKind::Assign(lhs, a, b) => ExprKind::Assign(
                Box::new(self.go(lhs, map)),
                Box::new(self.go(a, map)),
                Box::new(self.go(b, map)),
            ),
            ExprKind::Let(n, ty, a, b) => {
                let (inner, renamed) = self.scope(map, std::slice::from_ref(n));
                ExprKind::Let(
                    known_as(&renamed, n),
                    ty.clone(),
                    Box::new(self.go(a, map)),
                    Box::new(self.go(b, &inner)),
                )
            }
            ExprKind::LetMut(n, ty, a, b) => {
                let (inner, renamed) = self.scope(map, std::slice::from_ref(n));
                ExprKind::LetMut(
                    known_as(&renamed, n),
                    ty.clone(),
                    Box::new(self.go(a, map)),
                    Box::new(self.go(b, &inner)),
                )
            }
            ExprKind::For(v, index, iter, b) => {
                // Both binders shadow a parameter of the same name.
                let (inner, renamed) = self.scope(map, &crate::loop_binders(v, index));
                ExprKind::For(
                    known_as(&renamed, v),
                    index.as_ref().map(|i| known_as(&renamed, i)),
                    Box::new(self.go(iter, map)),
                    Box::new(self.go(b, &inner)),
                )
            }
            ExprKind::Match(s, arms) => ExprKind::Match(
                Box::new(self.go(s, map)),
                arms.iter().map(|a| self.arm(a, map)).collect(),
            ),
            ExprKind::IfLet(arm, s, else_b) => ExprKind::IfLet(
                Box::new(self.arm(arm, map)),
                Box::new(self.go(s, map)),
                Box::new(self.go(else_b, map)),
            ),
            ExprKind::Lambda(ps, b) => {
                let names: Vec<String> = ps.iter().map(|p| p.name.clone()).collect();
                let (inner, renamed) = self.scope(map, &names);
                let ps = ps
                    .iter()
                    .map(|p| LambdaParam {
                        name: known_as(&renamed, &p.name),
                        ..p.clone()
                    })
                    .collect();
                ExprKind::Lambda(ps, Box::new(self.go(b, &inner)))
            }
        };
        Expr::rebuilt(kind, e)
    }

    /// A match arm: its pattern's binders open a scope over the body.
    fn arm(&mut self, arm: &MatchArm, map: &HashMap<String, String>) -> MatchArm {
        let (inner, renamed) = self.scope(map, &arm.pattern.bindings());
        MatchArm {
            pattern: arm
                .pattern
                .rename_bindings(&mut |n| renamed.get(n).cloned().unwrap_or_else(|| n.to_string())),
            body: self.go(&arm.body, &inner),
            span: arm.span.clone(),
            written: WrittenNames {
                cases: arm.written.cases.clone(),
                binders: arm
                    .written
                    .binders
                    .iter()
                    .map(|b| {
                        SpanStr::new(
                            renamed
                                .get(&b.text)
                                .cloned()
                                .unwrap_or_else(|| b.text.clone()),
                            b.span.clone(),
                        )
                    })
                    .collect(),
            },
        }
    }
}
