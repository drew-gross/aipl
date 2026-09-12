//! Nested patterns: the matrix algorithm behind exhaustiveness, and the shape
//! both the checker and the decision-tree compiler work on.
//!
//! A pattern as the parser hands it over (`ast::Pattern`) is either one of the
//! simple forms codegen knows — a constructor over plain binders, a literal, a
//! wildcard — or a nested one (`Tuple`, `Nested`, `Int`, `Bind`). Here both
//! are read as one thing, a
//! [`Pat`]: a constructor over sub-patterns, a literal, or a wildcard. A tuple
//! is a constructor too (the one constructor of its type, named
//! [`TUPLE`]), and a binder is a wildcard that also names its value — which is
//! all a matrix algorithm needs to know about it.
//!
//! # Usefulness
//!
//! Exhaustiveness and redundancy are the same question asked twice (Maranget,
//! *Warnings for pattern matching*): a row of patterns `q` is *useful* against
//! a matrix `P` when some value matches `q` and no row of `P`. A match is
//! exhaustive when the all-wildcard row is *not* useful against its arms, and
//! an arm is redundant when its own row is not useful against the arms above
//! it. [`useful`] answers with a witness — a value shape that reaches `q` — so
//! a non-exhaustive match can name what it misses.
//!
//! The algorithm is the textbook one. Look at the first column: if `q` starts
//! with a constructor, specialize the matrix to that constructor (keep the rows
//! that start with it or with a wildcard, replacing the head by its
//! sub-patterns) and recurse; if `q` starts with a wildcard, either the
//! constructors already in the column form the type's *complete* signature —
//! then `q` is useful iff it is useful under some one of them — or they don't,
//! and `q` is useful iff it is useful against the *default* matrix (the rows
//! that start with a wildcard, heads dropped). What "complete" means is the
//! type's business, asked through [`Signature`]: a tuple has one constructor,
//! an optional two, a variant its cases, and a scalar's literals never complete
//! anything, so a literal match needs a wildcard to be exhaustive.

use aipl_syntax::ast::Type;

/// The constructor name a tuple pattern is read as. Not spellable in source,
/// so it can never collide with a case.
pub(crate) const TUPLE: &str = "(tuple)";

/// A literal a pattern matches by value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lit {
    Int(i64),
    Str(String),
    Char(u8),
}

/// A pattern as the matrix algorithm sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pat {
    /// Anything — `_`, or a binder, whose name is kept only so a witness can
    /// be rendered as the user might write it.
    Wild,
    /// A constructor with its sub-patterns; a tuple is [`TUPLE`] over its
    /// elements, a nullary case has none.
    Ctor(String, Vec<Pat>),
    Lit(Lit),
}

/// What a column's type says about its constructors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Signature {
    /// Every constructor the type has, each with its payload's types. A column
    /// whose patterns mention all of them is covered without a wildcard.
    Complete(Vec<(String, Vec<Type>)>),
    /// A scalar matched by literal: no finite set of literals covers it.
    Open,
}

/// What the matrix algorithm has to ask a type: its signature. Implemented by
/// the checker over its declarations, so this module needs none of them.
pub(crate) trait TypeInfo {
    fn signature(&self, ty: &Type) -> Signature;
}

/// One row of the matrix: a pattern per column.
pub(crate) type Row = Vec<Pat>;

/// The head constructor of a pattern, if it has one.
fn head_ctor(p: &Pat) -> Option<&str> {
    match p {
        Pat::Ctor(name, _) => Some(name),
        Pat::Wild | Pat::Lit(_) => None,
    }
}

/// Specialize `rows` to constructor `ctor` of `arity`: keep the rows whose
/// first pattern is that constructor or a wildcard, and replace the first
/// pattern by its sub-patterns (`arity` wildcards for a wildcard).
fn specialize(rows: &[Row], ctor: &str, arity: usize) -> Vec<Row> {
    rows.iter()
        .filter_map(|row| {
            let (head, rest) = row.split_first()?;
            let subs: Vec<Pat> = match head {
                Pat::Ctor(name, args) if name == ctor => args.clone(),
                Pat::Wild => vec![Pat::Wild; arity],
                Pat::Ctor(..) | Pat::Lit(_) => return None,
            };
            Some(subs.into_iter().chain(rest.iter().cloned()).collect())
        })
        .collect()
}

/// Specialize `rows` to the literal `lit`: the rows whose first pattern is
/// that literal or a wildcard, heads dropped.
fn specialize_lit(rows: &[Row], lit: &Lit) -> Vec<Row> {
    rows.iter()
        .filter_map(|row| {
            let (head, rest) = row.split_first()?;
            match head {
                Pat::Lit(l) if l == lit => Some(rest.to_vec()),
                Pat::Wild => Some(rest.to_vec()),
                Pat::Lit(_) | Pat::Ctor(..) => None,
            }
        })
        .collect()
}

/// The default matrix: the rows whose first pattern is a wildcard, heads
/// dropped.
fn default(rows: &[Row]) -> Vec<Row> {
    rows.iter()
        .filter_map(|row| match row.split_first() {
            Some((Pat::Wild, rest)) => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

/// Whether `q` is useful against `rows` over columns typed `tys` — and if so,
/// a witness: one value shape (as patterns) that `q` matches and no row does.
/// See the module docs.
pub(crate) fn useful(rows: &[Row], q: &[Pat], tys: &[Type], info: &dyn TypeInfo) -> Option<Row> {
    if rows.is_empty() {
        // Nothing above matches anything: `q` itself is the witness, with its
        // wildcards left open.
        return Some(q.to_vec());
    }
    let Some((head, q_rest)) = q.split_first() else {
        // No columns left and some row remains: that row matched first.
        return None;
    };
    let (ty, tys_rest) = tys.split_first().expect("one type per column");
    match head {
        Pat::Ctor(name, args) => {
            let arity = args.len();
            let sub_tys = ctor_types(info, ty, name, arity);
            let spec_q: Vec<Pat> = args.iter().cloned().chain(q_rest.iter().cloned()).collect();
            let spec_tys: Vec<Type> = sub_tys
                .iter()
                .cloned()
                .chain(tys_rest.iter().cloned())
                .collect();
            let w = useful(&specialize(rows, name, arity), &spec_q, &spec_tys, info)?;
            Some(rebuild_ctor(name, arity, w))
        }
        Pat::Lit(lit) => {
            let w = useful(&specialize_lit(rows, lit), q_rest, tys_rest, info)?;
            Some(std::iter::once(Pat::Lit(lit.clone())).chain(w).collect())
        }
        Pat::Wild => {
            let heads: Vec<&str> = rows
                .iter()
                .filter_map(|r| r.first().and_then(head_ctor))
                .collect();
            let complete = match info.signature(ty) {
                Signature::Complete(ctors) if !ctors.is_empty() => {
                    let all = ctors.iter().all(|(c, _)| heads.contains(&c.as_str()));
                    all.then_some(ctors)
                }
                Signature::Complete(_) | Signature::Open => None,
            };
            match complete {
                Some(ctors) => {
                    // Every constructor is present: `q` is useful under some one
                    // of them, and the witness names that one.
                    for (name, sub_tys) in &ctors {
                        let arity = sub_tys.len();
                        let spec_q: Vec<Pat> = std::iter::repeat_n(Pat::Wild, arity)
                            .chain(q_rest.iter().cloned())
                            .collect();
                        let spec_tys: Vec<Type> = sub_tys
                            .iter()
                            .cloned()
                            .chain(tys_rest.iter().cloned())
                            .collect();
                        if let Some(w) =
                            useful(&specialize(rows, name, arity), &spec_q, &spec_tys, info)
                        {
                            return Some(rebuild_ctor(name, arity, w));
                        }
                    }
                    None
                }
                None => {
                    // Some constructor (or every literal) is missing: a value
                    // outside the ones the column names reaches the default
                    // rows. The witness is such a constructor when one is
                    // nameable, else a wildcard.
                    let w = useful(&default(rows), q_rest, tys_rest, info)?;
                    let missing = match info.signature(ty) {
                        Signature::Complete(ctors) => ctors
                            .iter()
                            .find(|(c, _)| !heads.contains(&c.as_str()))
                            .map(|(c, sub)| Pat::Ctor(c.clone(), vec![Pat::Wild; sub.len()])),
                        Signature::Open => None,
                    };
                    Some(
                        std::iter::once(missing.unwrap_or(Pat::Wild))
                            .chain(w)
                            .collect(),
                    )
                }
            }
        }
    }
}

/// The payload types of constructor `name` at `ty`, `arity` wildcards' worth of
/// unknowns when the type does not know it (the checker has already refused
/// that pattern; the algorithm just has to keep going).
fn ctor_types(info: &dyn TypeInfo, ty: &Type, name: &str, arity: usize) -> Vec<Type> {
    if let Signature::Complete(ctors) = info.signature(ty) {
        if let Some((_, sub)) = ctors.into_iter().find(|(c, _)| c == name) {
            if sub.len() == arity {
                return sub;
            }
        }
    }
    vec![Type::Any; arity]
}

/// Fold the first `arity` patterns of a witness back under the constructor
/// they were specialized out of.
fn rebuild_ctor(name: &str, arity: usize, mut w: Row) -> Row {
    let rest = w.split_off(arity.min(w.len()));
    std::iter::once(Pat::Ctor(name.to_string(), w))
        .chain(rest)
        .collect()
}

/// A witness rendered the way its user would write it.
pub(crate) fn render(p: &Pat) -> String {
    match p {
        Pat::Wild => "_".to_string(),
        Pat::Lit(Lit::Int(n)) => n.to_string(),
        Pat::Lit(Lit::Str(s)) => format!("{s:?}"),
        Pat::Lit(Lit::Char(c)) => format!("'{}'", char::from(*c)),
        Pat::Ctor(name, args) if name == TUPLE => {
            format!(
                "({})",
                args.iter().map(render).collect::<Vec<_>>().join(", ")
            )
        }
        Pat::Ctor(name, args) => {
            let bare = name.split('@').next().unwrap_or(name);
            if args.is_empty() {
                bare.to_string()
            } else {
                format!(
                    "{bare}({})",
                    args.iter().map(render).collect::<Vec<_>>().join(", ")
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aipl_syntax::ast::Primitive;

    /// A world with one type: `i64?`, and tuples of it.
    struct Opt;
    impl TypeInfo for Opt {
        fn signature(&self, ty: &Type) -> Signature {
            match ty {
                Type::Optional(inner) => Signature::Complete(vec![
                    ("some".into(), vec![(**inner).clone()]),
                    ("none".into(), vec![]),
                ]),
                Type::Tuple(es) => Signature::Complete(vec![(TUPLE.into(), es.clone())]),
                _ => Signature::Open,
            }
        }
    }

    fn i64_ty() -> Type {
        Type::Primitive(Primitive::I64)
    }
    fn some(p: Pat) -> Pat {
        Pat::Ctor("some".into(), vec![p])
    }
    fn none() -> Pat {
        Pat::Ctor("none".into(), vec![])
    }
    fn lit(n: i64) -> Pat {
        Pat::Lit(Lit::Int(n))
    }
    fn tup(ps: Vec<Pat>) -> Pat {
        Pat::Ctor(TUPLE.into(), ps)
    }

    /// `rep_suffix`'s own match, over `(i64, i64?)`.
    fn rep_rows() -> Vec<Row> {
        vec![
            vec![tup(vec![lit(1), some(lit(1))])],
            vec![tup(vec![lit(0), some(lit(1))])],
            vec![tup(vec![Pat::Wild, some(Pat::Wild)])],
            vec![tup(vec![lit(0), none()])],
            vec![tup(vec![lit(1), none()])],
            vec![tup(vec![Pat::Wild, none()])],
        ]
    }

    fn rep_ty() -> Vec<Type> {
        vec![Type::Tuple(vec![
            i64_ty(),
            Type::Optional(Box::new(i64_ty())),
        ])]
    }

    #[test]
    fn rep_suffix_is_exhaustive_and_every_arm_reachable() {
        let rows = rep_rows();
        assert_eq!(useful(&rows, &[Pat::Wild], &rep_ty(), &Opt), None);
        for i in 0..rows.len() {
            assert!(
                useful(&rows[..i], &rows[i], &rep_ty(), &Opt).is_some(),
                "arm {i}"
            );
        }
    }

    #[test]
    fn a_missing_case_is_named() {
        let rows: Vec<Row> = rep_rows().into_iter().take(3).collect();
        let w = useful(&rows, &[Pat::Wild], &rep_ty(), &Opt).expect("not exhaustive");
        assert_eq!(render(&w[0]), "(_, none)");
    }

    #[test]
    fn a_literal_column_needs_a_wildcard() {
        let rows = vec![vec![lit(1)], vec![lit(2)]];
        let w = useful(&rows, &[Pat::Wild], &[i64_ty()], &Opt).expect("open domain");
        assert_eq!(render(&w[0]), "_");
        let rows = vec![vec![lit(1)], vec![Pat::Wild]];
        assert_eq!(useful(&rows, &[Pat::Wild], &[i64_ty()], &Opt), None);
    }

    #[test]
    fn a_shadowed_arm_is_redundant() {
        let rows = vec![vec![Pat::Wild], vec![lit(1)]];
        assert_eq!(useful(&rows[..1], &rows[1], &[i64_ty()], &Opt), None);
    }
}
