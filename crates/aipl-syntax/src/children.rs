//! The immediate sub-expressions of an expression, as iterators that allocate nothing.
//!
//! [`Expr::children`] is the one list of what nests inside each node, so a walk that
//! only needs "every child" (does this contain a `return`? does it mention a name?) does
//! not write the exhaustive `match` itself, and does not pay a `Vec` per node to get it.
//! The iterators borrow from the node, so a child is `&Expr` for the node's lifetime.
//!
//! The order is evaluation order, and every child is yielded — an `Assign`'s target
//! included, since before inference it can hold expressions of its own. A pass that
//! treats the target as a name, not a child, skips it (`aipl_mono::children`).

use crate::ast::{Expr, ExprKind, FieldInit, MatchArm};
use std::slice::{Iter, IterMut};

/// The children of an [`Expr`]; see [`Expr::children`].
pub struct Children<'a>(Shape<'a>);

enum Shape<'a> {
    /// Up to three children held inline, yielded in order.
    Fixed([Option<&'a Expr>; 3], usize),
    /// A list of expressions: a call's arguments, an array's elements.
    List(Iter<'a, Expr>),
    /// A dict literal's keys and values, alternately; the second is the value still owed.
    Pairs(Iter<'a, (Expr, Expr)>, Option<&'a Expr>),
    /// A construction's field values.
    Inits(Iter<'a, FieldInit>),
    /// A `match`: the scrutinee, then each arm's body.
    Arms(Option<&'a Expr>, Iter<'a, MatchArm>),
}

impl<'a> Children<'a> {
    /// No children.
    pub fn none() -> Self {
        Children(Shape::Fixed([None; 3], 0))
    }

    /// Exactly these children, in order.
    pub fn of<const N: usize>(children: [&'a Expr; N]) -> Self {
        debug_assert!(N <= 3);
        let mut fixed = [None; 3];
        for (slot, child) in fixed.iter_mut().zip(children) {
            *slot = Some(child);
        }
        Children(Shape::Fixed(fixed, 0))
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = &'a Expr;

    fn next(&mut self) -> Option<&'a Expr> {
        match &mut self.0 {
            Shape::Fixed(items, next) => {
                let item = items.get(*next).copied().flatten();
                *next += 1;
                item.or_else(|| {
                    // Slots fill from the front, so the first empty one ends it.
                    *next = items.len();
                    None
                })
            }
            Shape::List(items) => items.next(),
            Shape::Pairs(pairs, owed) => {
                if let Some(value) = owed.take() {
                    return Some(value);
                }
                let (key, value) = pairs.next()?;
                *owed = Some(value);
                Some(key)
            }
            Shape::Inits(inits) => inits.next().map(|init| &init.value),
            Shape::Arms(scrutinee, arms) => match scrutinee.take() {
                Some(scrutinee) => Some(scrutinee),
                None => arms.next().map(|arm| &arm.body),
            },
        }
    }
}

/// The children of an [`Expr`], mutably; see [`Expr::children_mut`].
pub struct ChildrenMut<'a>(MutShape<'a>);

enum MutShape<'a> {
    Fixed([Option<&'a mut Expr>; 3], usize),
    List(IterMut<'a, Expr>),
    Pairs(IterMut<'a, (Expr, Expr)>, Option<&'a mut Expr>),
    Inits(IterMut<'a, FieldInit>),
    Arms(Option<&'a mut Expr>, IterMut<'a, MatchArm>),
}

impl<'a> ChildrenMut<'a> {
    /// No children.
    pub fn none() -> Self {
        ChildrenMut(MutShape::Fixed([None, None, None], 0))
    }

    /// Exactly these children, in order.
    pub fn of<const N: usize>(children: [&'a mut Expr; N]) -> Self {
        debug_assert!(N <= 3);
        let mut fixed = [None, None, None];
        for (slot, child) in fixed.iter_mut().zip(children) {
            *slot = Some(child);
        }
        ChildrenMut(MutShape::Fixed(fixed, 0))
    }
}

impl<'a> Iterator for ChildrenMut<'a> {
    type Item = &'a mut Expr;

    fn next(&mut self) -> Option<&'a mut Expr> {
        match &mut self.0 {
            MutShape::Fixed(items, next) => {
                let item = items.get_mut(*next).and_then(Option::take);
                *next += 1;
                item
            }
            MutShape::List(items) => items.next(),
            MutShape::Pairs(pairs, owed) => {
                if let Some(value) = owed.take() {
                    return Some(value);
                }
                let (key, value) = pairs.next()?;
                *owed = Some(value);
                Some(key)
            }
            MutShape::Inits(inits) => inits.next().map(|init| &mut init.value),
            MutShape::Arms(scrutinee, arms) => match scrutinee.take() {
                Some(scrutinee) => Some(scrutinee),
                None => arms.next().map(|arm| &mut arm.body),
            },
        }
    }
}

impl Expr {
    /// The immediate sub-expressions of `self`, in evaluation order. An `Assign`'s
    /// target is among them; a binder's name and a pattern are not expressions.
    pub fn children(&self) -> Children<'_> {
        use ExprKind as K;
        match &self.kind {
            K::Num(_)
            | K::Float(_)
            | K::Bool(_)
            | K::Str(_)
            | K::Char(_)
            | K::Ident(_)
            | K::None
            | K::Unit => Children::none(),
            K::Shim(_, _, x)
            | K::Neg(x)
            | K::Field(x, _)
            | K::Try(x)
            | K::Return(x)
            | K::KwArg(_, x, _)
            | K::TypeArgs(x, _)
            | K::Spread(x)
            | K::Lambda(_, x) => Children::of([x]),
            K::Seq(a, b)
            | K::Index(a, b)
            | K::Let(_, _, a, b)
            | K::LetMut(_, _, a, b)
            | K::For(_, _, a, b)
            | K::While(a, b) => Children::of([a, b]),
            K::Assign(a, b, c) | K::If(a, b, c) => Children::of([a, b, c]),
            K::Slice(a, b, c) => Children(Shape::Fixed([Some(a), Some(b), c.as_deref()], 0)),
            K::Call(_, items, _)
            | K::ArrayLit(items)
            | K::SetLit(items, _)
            | K::TupleLit(items) => Children(Shape::List(items.iter())),
            K::DictLit(pairs) => Children(Shape::Pairs(pairs.iter(), None)),
            K::Construct(_, inits) => Children(Shape::Inits(inits.iter())),
            K::Match(scrutinee, arms) => Children(Shape::Arms(Some(scrutinee), arms.iter())),
            K::IfLet(arm, scrutinee, else_branch) => {
                Children::of([scrutinee, &arm.body, else_branch])
            }
        }
    }

    /// [`Expr::children`], mutably.
    pub fn children_mut(&mut self) -> ChildrenMut<'_> {
        use ExprKind as K;
        match &mut self.kind {
            K::Num(_)
            | K::Float(_)
            | K::Bool(_)
            | K::Str(_)
            | K::Char(_)
            | K::Ident(_)
            | K::None
            | K::Unit => ChildrenMut::none(),
            K::Shim(_, _, x)
            | K::Neg(x)
            | K::Field(x, _)
            | K::Try(x)
            | K::Return(x)
            | K::KwArg(_, x, _)
            | K::TypeArgs(x, _)
            | K::Spread(x)
            | K::Lambda(_, x) => ChildrenMut::of([x]),
            K::Seq(a, b)
            | K::Index(a, b)
            | K::Let(_, _, a, b)
            | K::LetMut(_, _, a, b)
            | K::For(_, _, a, b)
            | K::While(a, b) => ChildrenMut::of([a, b]),
            K::Assign(a, b, c) | K::If(a, b, c) => ChildrenMut::of([a, b, c]),
            K::Slice(a, b, c) => {
                ChildrenMut(MutShape::Fixed([Some(a), Some(b), c.as_deref_mut()], 0))
            }
            K::Call(_, items, _)
            | K::ArrayLit(items)
            | K::SetLit(items, _)
            | K::TupleLit(items) => ChildrenMut(MutShape::List(items.iter_mut())),
            K::DictLit(pairs) => ChildrenMut(MutShape::Pairs(pairs.iter_mut(), None)),
            K::Construct(_, inits) => ChildrenMut(MutShape::Inits(inits.iter_mut())),
            K::Match(scrutinee, arms) => {
                ChildrenMut(MutShape::Arms(Some(scrutinee), arms.iter_mut()))
            }
            K::IfLet(arm, scrutinee, else_branch) => {
                ChildrenMut::of([scrutinee, &mut arm.body, else_branch])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::{Callee, Expr, ExprKind};

    fn ident(name: &str) -> Expr {
        Expr::new(ExprKind::Ident(name.to_string()), 0..0)
    }

    fn names<'a>(children: impl Iterator<Item = &'a Expr>) -> Vec<&'a str> {
        children
            .map(|c| match &c.kind {
                ExprKind::Ident(n) => n.as_str(),
                _ => "?",
            })
            .collect()
    }

    #[test]
    fn a_leaf_has_none_and_a_call_has_its_arguments() {
        assert_eq!(names(ident("x").children()), Vec::<&str>::new());
        let call = Expr::new(
            ExprKind::Call(
                Callee::User("f".to_string()),
                vec![ident("a"), ident("b")],
                false,
            ),
            0..0,
        );
        assert_eq!(names(call.children()), ["a", "b"]);
    }

    #[test]
    fn fixed_children_come_in_order_and_a_missing_slice_end_is_skipped() {
        let slice = Expr::new(
            ExprKind::Slice(Box::new(ident("r")), Box::new(ident("lo")), None),
            0..0,
        );
        assert_eq!(names(slice.children()), ["r", "lo"]);
        let assign = Expr::new(
            ExprKind::Assign(
                Box::new(ident("t")),
                Box::new(ident("v")),
                Box::new(ident("b")),
            ),
            0..0,
        );
        assert_eq!(names(assign.children()), ["t", "v", "b"]);
    }

    #[test]
    fn a_dict_yields_its_keys_and_values_alternately_and_the_mutable_form_agrees() {
        let mut dict = Expr::new(
            ExprKind::DictLit(vec![(ident("k1"), ident("v1")), (ident("k2"), ident("v2"))]),
            0..0,
        );
        assert_eq!(names(dict.children()), ["k1", "v1", "k2", "v2"]);
        let mut seen = Vec::new();
        for child in dict.children_mut() {
            if let ExprKind::Ident(n) = &mut child.kind {
                seen.push(n.clone());
            }
        }
        assert_eq!(seen, ["k1", "v1", "k2", "v2"]);
    }
}
