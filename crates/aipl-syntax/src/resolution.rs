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
pub fn settled(found: Vec<Resolution>) -> Vec<Resolution> {
    let mut out: Vec<Resolution> = Vec::new();
    let mut conflicted: Vec<(String, Span)> = Vec::new();
    for r in found {
        if conflicted.iter().any(|(f, s)| *f == r.in_fn && *s == r.at) {
            continue;
        }
        match out.iter().position(|o| o.in_fn == r.in_fn && o.at == r.at) {
            Some(i) if out[i].target == r.target => {}
            Some(i) => {
                out.remove(i);
                conflicted.push((r.in_fn, r.at));
            }
            None => out.push(r),
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
    fn a_name_that_resolved_two_ways_is_dropped_for_good() {
        let a = found(1..2, Target::Function("g".to_string()));
        let b = found(1..2, Target::Function("h".to_string()));
        let c = found(3..4, Target::Struct("S".to_string()));
        assert_eq!(settled(vec![a.clone(), b, a, c.clone()]), vec![c]);
    }
}
