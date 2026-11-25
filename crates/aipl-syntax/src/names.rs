//! Names for the locals a pass introduces, unique within one function.
//!
//! An optimization that copies a callee's body into a caller, or builds a loop around a
//! pipeline, needs binders that nothing around them is already called. A [`Names`] is the supply
//! such a pass draws on: it starts from the names a function already holds, and each name it
//! hands out is recorded, so the next one differs from it. What it returns depends on the
//! function and on the requests made of it, and on nothing else — not on another function, another
//! thread, or the order files were compiled in — so the compiler's output is the same each time.
//!
//! A generated name is `$<stem><n>` or `$<stem><n>_<base>`. A `$` cannot appear in a source
//! identifier, so a generated name cannot collide with one a person wrote. The `<stem><n>` part is
//! the unit of uniqueness: one `n` is used for one stem once, and every name that shares it
//! (`$fuse3_src`, `$fuse3_left`) is therefore free.

use std::collections::HashSet;

use crate::ast::{Expr, ExprKind};
use crate::typed::{TExpr, TExprKind, TyRepr};

/// The names a function holds, and a cursor past the numbers handed out so far.
#[derive(Debug, Clone, Default)]
pub struct Names {
    /// Every `$<stem><n>` that is spoken for: each prefix of a taken name that ends where a `_`
    /// begins, and the whole name.
    keys: HashSet<String>,
    /// The next number to try. Only grows, so the supply never rescans what it has handed out.
    next: usize,
}

impl Names {
    /// A supply for a function that holds no generated names yet.
    pub fn new() -> Names {
        Names::default()
    }

    /// A supply for the function whose body is `body`.
    pub fn of_expr(body: &Expr) -> Names {
        let mut names = Names::new();
        names.reserve_expr(body);
        names
    }

    /// A supply for the function whose body is `body`.
    pub fn of_typed<T: TyRepr>(body: &TExpr<T>) -> Names {
        let mut names = Names::new();
        names.reserve_typed(body);
        names
    }

    /// Count every name `body` holds as taken.
    pub fn reserve_expr(&mut self, body: &Expr) {
        crate::each_subexpr(body, &mut |e| match &e.kind {
            ExprKind::Ident(n) => self.reserve(n),
            ExprKind::Call(callee, ..) => self.reserve(callee.name()),
            ExprKind::Let(n, ..) | ExprKind::LetMut(n, ..) => self.reserve(n),
            ExprKind::For(n, index, ..) => {
                self.reserve(n);
                if let Some(index) = index {
                    self.reserve(index);
                }
            }
            ExprKind::Lambda(params, _) => {
                for p in params {
                    self.reserve(&p.name);
                }
            }
            ExprKind::Match(_, arms) => {
                for arm in arms {
                    self.reserve_all(arm.pattern.bindings());
                }
            }
            ExprKind::IfLet(arm, ..) => self.reserve_all(arm.pattern.bindings()),
            ExprKind::Shim(_, bindings, _) => {
                for (_, f) in bindings {
                    self.reserve(f);
                }
            }
            _ => {}
        });
    }

    /// Count every name `body` holds as taken.
    pub fn reserve_typed<T: TyRepr>(&mut self, body: &TExpr<T>) {
        body.walk(&mut |e| match &e.kind {
            TExprKind::Ident(n) => self.reserve(n),
            TExprKind::Call { callee, .. } => self.reserve(callee.name()),
            TExprKind::Let(n, ..) | TExprKind::LetMut(n, ..) => self.reserve(n),
            TExprKind::For(n, index, ..) => {
                self.reserve(n);
                if let Some(index) = index {
                    self.reserve(index);
                }
            }
            TExprKind::Lambda(params, _) => {
                for p in params {
                    self.reserve(&p.name);
                }
            }
            TExprKind::Match(_, arms) => {
                for arm in arms {
                    self.reserve_all(arm.pattern.bindings());
                }
            }
            TExprKind::IfLet(arm, ..) => self.reserve_all(arm.pattern.bindings()),
            TExprKind::Shim(_, bindings, _) => {
                for (_, f) in bindings {
                    self.reserve(f);
                }
            }
            _ => {}
        });
    }

    fn reserve_all(&mut self, names: Vec<String>) {
        for n in names {
            self.reserve(&n);
        }
    }

    /// Count `name` as taken. A name with no `$` is a person's, which no generated name can equal.
    pub fn reserve(&mut self, name: &str) {
        // A shim's closure is found by its prefix, which a fresh name keeps (see `fresh`).
        let name = name
            .strip_prefix(crate::SHIM_CLOSURE_PREFIX)
            .map_or(name, |rest| rest);
        if !name.contains('$') {
            return;
        }
        for (at, _) in name.match_indices('_') {
            self.keys.insert(name[..at].to_string());
        }
        self.keys.insert(name.to_string());
    }

    /// The `<stem><n>` of a name nothing else is called, now spoken for: `$<stem><n>`.
    pub fn group(&mut self, stem: &str) -> String {
        loop {
            let key = format!("${stem}{}", self.next);
            self.next += 1;
            if self.keys.insert(key.clone()) {
                return key;
            }
        }
    }

    /// A temporary nothing else is called: `$<stem><n>`.
    pub fn temp(&mut self, stem: &str) -> String {
        self.group(stem)
    }

    /// A local nothing else is called: `$<stem><n>_<base>`.
    pub fn fresh(&mut self, stem: &str, base: &str) -> String {
        format!("{}_{base}", self.group(stem))
    }

    /// [`Names::fresh`] for a binder that may be a shim's closure, which is found by its prefix
    /// and so keeps it.
    pub fn fresh_binder(&mut self, stem: &str, base: &str) -> String {
        match base.strip_prefix(crate::SHIM_CLOSURE_PREFIX) {
            Some(rest) => format!("{}{}", crate::SHIM_CLOSURE_PREFIX, self.fresh(stem, rest)),
            None => self.fresh(stem, base),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_name_differs_from_every_earlier_one() {
        let mut names = Names::new();
        assert_eq!(names.fresh("inl", "x"), "$inl0_x");
        assert_eq!(names.fresh("inl", "x"), "$inl1_x");
        assert_eq!(names.temp("cell"), "$cell2");
    }

    #[test]
    fn a_name_the_function_holds_is_never_handed_out() {
        let mut names = Names::new();
        names.reserve("$inl0_x");
        names.reserve("$cell1");
        names.reserve("x");
        // The numbers are shared across stems, so a number taken under one is skipped for all.
        assert_eq!(names.fresh("inl", "x"), "$inl1_x");
        assert_eq!(names.temp("cell"), "$cell2");
    }

    #[test]
    fn a_taken_name_spends_its_whole_group() {
        let mut names = Names::new();
        names.reserve("$fuse0_src");
        // `$fuse0` is spoken for, so no `$fuse0_<anything>` can be handed out.
        assert_eq!(names.group("fuse"), "$fuse1");
    }

    #[test]
    fn a_stem_with_an_underscore_is_told_from_its_number() {
        let mut names = Names::new();
        names.reserve("$mut_copy0");
        assert_eq!(names.temp("mut_copy"), "$mut_copy1");
    }

    #[test]
    fn a_shims_closure_name_keeps_its_prefix() {
        let mut names = Names::new();
        let fresh = names.fresh_binder("inl", "$shimfn$print");
        assert_eq!(fresh, "$shimfn$$inl0_print");
        names.reserve(&fresh);
        assert_eq!(names.fresh("inl", "print"), "$inl1_print");
    }

    #[test]
    fn the_same_requests_give_the_same_names() {
        let ask = || {
            let mut names = Names::new();
            names.reserve("$inl1_a");
            [
                names.fresh("inl", "a"),
                names.temp("cell"),
                names.fresh("ext", "x"),
            ]
        };
        assert_eq!(ask(), ask());
    }
}
