//! What the checker decided each written name refers to.
//!
//! The checker already answers "which struct is this field read off", "which
//! function does this call reach" and "which binding is this identifier" in
//! order to type a program, and then forgets. A language server wants exactly
//! those answers, and wants them from the compiler rather than from a second
//! implementation of its scoping and typing rules that could disagree with it.
//! So the checker records them here as it goes.
//!
//! A [`Resolution`] is keyed by the span of the *name* (see `Expr::name_span`)
//! and the function whose body it sits in, because a span is an offset within
//! its own file and the same offset exists in every file of a program.

use crate::Span;

/// Where a written name points. Names are the loader's global (mangled) ones, so
/// the file a declaration lives in is recoverable from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Field `field` of struct `owner` — the struct's declaring item, which for
    /// a generic instance is its template.
    Field { owner: String, field: String },
    /// A struct, by the name of its declaring item.
    Struct(String),
    /// A function (or builtin), by its global name.
    Function(String),
    /// A case of a variant, written as a constructor or in a pattern: the
    /// declaring variant item (the template, for a generic one) and the bare
    /// case name.
    Case { variant: String, case: String },
    /// The named payload slot `field` of case `case` of a variant, as `value.field` reads it
    /// off a value known to be that case: the declaring variant item (the template, for a
    /// generic one) and the bare case name.
    CaseField {
        variant: String,
        case: String,
        field: String,
    },
    /// A local binding (`let`, `mut`, `for`), by the span of the name that
    /// introduced it in the same function.
    Local(Span),
}

/// One written name and what it was resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The global name of the function whose body holds the name.
    pub in_fn: String,
    /// Where the name is written.
    pub at: Span,
    pub target: Target,
}

/// Collapse repeated findings for one written name. The checker may reach an
/// expression more than once (a lambda body is checked per candidate type), and
/// a name that resolved two different ways is dropped: a wrong answer is worse
/// than a missing one.
///
/// A name may keep several answers when each is a local, and one more when it is a
/// field. A struct literal's shorthand `T { kind }` writes the field it initializes and
/// the binding it reads with the same characters; and a use in an arm shared by
/// alternatives (`A(x) | B(x) => x`) reads the binder of each.
pub fn settled(found: Vec<Resolution>) -> Vec<Resolution> {
    let mut out: Vec<Resolution> = Vec::new();
    let mut conflicted: Vec<(String, Span)> = Vec::new();
    let compatible = |a: &Target, b: &Target| {
        matches!(
            (a, b),
            (Target::Local(_), Target::Local(_))
                | (Target::Field { .. }, Target::Local(_))
                | (Target::Local(_), Target::Field { .. })
        )
    };
    for r in found {
        if conflicted.iter().any(|(f, s)| *f == r.in_fn && *s == r.at) {
            continue;
        }
        let same: Vec<usize> = (0..out.len())
            .filter(|&i| out[i].in_fn == r.in_fn && out[i].at == r.at)
            .collect();
        if same.iter().any(|&i| out[i].target == r.target) {
            continue;
        }
        if same.iter().all(|&i| compatible(&out[i].target, &r.target)) {
            out.push(r);
        } else {
            out.retain(|o| !(o.in_fn == r.in_fn && o.at == r.at));
            conflicted.push((r.in_fn, r.at));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(at: Span, target: Target) -> Resolution {
        Resolution {
            in_fn: "f".to_string(),
            at,
            target,
        }
    }

    #[test]
    fn a_repeat_of_one_answer_is_kept_once() {
        let r = found(1..2, Target::Function("g".to_string()));
        assert_eq!(settled(vec![r.clone(), r.clone()]), vec![r]);
    }

    #[test]
    fn a_field_and_a_local_at_one_name_are_both_kept() {
        let field = found(
            1..2,
            Target::Field {
                owner: "S".to_string(),
                field: "x".to_string(),
            },
        );
        let local = found(1..2, Target::Local(7..8));
        assert_eq!(
            settled(vec![field.clone(), local.clone(), field.clone()]),
            vec![field.clone(), local.clone()]
        );
        // An answer of another kind spoils both.
        let other = found(1..2, Target::Function("g".to_string()));
        assert_eq!(settled(vec![field, local, other]), vec![]);
    }

    #[test]
    fn a_name_read_through_several_binders_keeps_each() {
        let first = found(1..2, Target::Local(7..8));
        let second = found(1..2, Target::Local(9..10));
        assert_eq!(
            settled(vec![first.clone(), second.clone(), first.clone()]),
            vec![first, second]
        );
        // Two fields are two answers for one name, and neither is kept.
        let field = |owner: &str| {
            found(
                1..2,
                Target::Field {
                    owner: owner.to_string(),
                    field: "x".to_string(),
                },
            )
        };
        assert_eq!(settled(vec![field("S"), field("T")]), vec![]);
    }

    #[test]
    fn a_name_that_resolved_two_ways_is_dropped_for_good() {
        let a = found(1..2, Target::Function("g".to_string()));
        let b = found(1..2, Target::Function("h".to_string()));
        let c = found(3..4, Target::Struct("S".to_string()));
        assert_eq!(settled(vec![a.clone(), b, a, c.clone()]), vec![c]);
    }
}
