//! The typed body monomorphization produces for each instance.

use aipl_syntax::ast::{ConcreteType, Type};
use aipl_syntax::typed::TExpr;

pub(crate) type Typed = TExpr<ConcreteType>;

/// The concrete form of a stamp, or `None` for one the representation cannot say.
pub(crate) fn concrete(t: &Type) -> Option<ConcreteType> {
    t.to_concrete()
}
