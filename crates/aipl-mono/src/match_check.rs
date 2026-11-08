//! The rules for `match` and `if let` patterns: typing a nested pattern, and proving a match
//! exhaustive with every arm reachable. They need only two questions about types (a variant's
//! cases, a tuple's elements), so they are written against [`MatchInfo`] and shared by whoever can
//! answer them.

use crate::check::{is_typevar, is_unit, tyname};
use crate::patterns;
use aipl_syntax::ast::{MatchArm, Pattern, Primitive, Type};
use aipl_syntax::{is_str_repr, Error, Span};
use std::collections::HashSet;

/// What the match rules ask of a type.
pub(crate) trait MatchInfo: patterns::TypeInfo {
    /// The cases of a variant type, each with its payload types.
    fn cases_of(&self, ty: &Type) -> Option<Vec<(String, Vec<Type>)>>;
    /// The element types of a tuple type.
    fn tuple_elems(&self, ty: &Type) -> Option<Vec<Type>>;
    /// A case of `ty` was named by a pattern (for the language server's resolutions).
    fn note_case(&self, _ty: &Type, _bare: &str) {}
}

/// Type a nested pattern against `ty`: the binders it introduces, each with
/// the type of the value it names, and the pattern as the matrix algorithm
/// reads it (`patterns::Pat`), built here because a `Ctor(..)`'s arity is
/// only known against the type.
///
/// The rules per form: a tuple pattern needs a tuple of that arity; a
/// constructor needs a case of its type with as many slots as it has
/// sub-patterns; a literal needs the scalar it spells; a binder or `_`
/// takes anything. A name in a nested slot is a binder unless it spells a
/// case (see `Pattern::normalized`), and a capitalised binder that names no
/// case of the type is refused rather than silently bound.
pub(crate) fn bind_nested(
    info: &dyn MatchInfo,
    pattern: &Pattern,
    ty: &Type,
    span: Span,
) -> Result<(patterns::Pat, Vec<(String, Type)>), Error> {
    let (pat, binders) = bind_nested_inner(info, pattern, ty, &span)?;
    let mut seen: Vec<&str> = Vec::new();
    for (name, _) in &binders {
        if seen.contains(&name.as_str()) {
            return Err(Error::at(
                format!("binder {name:?} appears more than once in this pattern"),
                span,
            ));
        }
        seen.push(name);
    }
    Ok((pat, binders))
}

/// The element type a `[..]` pattern's slots match against: an array's
/// element, or `char` for a `str`, whose destructuring reads it as one.
pub(crate) fn seq_elem_ty(ty: &Type) -> Option<Type> {
    match ty {
        Type::Array(e) => Some((**e).clone()),
        _ if is_str_repr(ty) => Some(Type::Primitive(Primitive::Char)),
        _ if is_typevar(ty) => Some(ty.clone()),
        _ => None,
    }
}

pub(crate) fn bind_nested_inner(
    info: &dyn MatchInfo,
    pattern: &Pattern,
    ty: &Type,
    span: &Span,
) -> Result<(patterns::Pat, Vec<(String, Type)>), Error> {
    use patterns::{Lit, Pat};
    Ok(match pattern {
        Pattern::Wildcard => (Pat::Wild, Vec::new()),
        Pattern::Bind(name) => (Pat::Wild, vec![(name.clone(), ty.clone())]),
        Pattern::Int(n) => {
            if !aipl_syntax::is_int_ty(ty) && !is_typevar(ty) {
                return Err(Error::at(
                    format!("integer pattern {n} matches an integer, not {}", tyname(ty)),
                    span.clone(),
                ));
            }
            (Pat::Lit(Lit::Int(*n)), Vec::new())
        }
        Pattern::Str(s) => {
            if !is_str_repr(ty) && !is_typevar(ty) {
                return Err(Error::at(
                    format!("string-literal pattern matches a str, not {}", tyname(ty)),
                    span.clone(),
                ));
            }
            (Pat::Lit(Lit::Str(s.clone())), Vec::new())
        }
        Pattern::Char(c) => {
            if !matches!(ty, Type::Primitive(Primitive::Char)) && !is_typevar(ty) {
                return Err(Error::at(
                    format!("char-literal pattern matches a char, not {}", tyname(ty)),
                    span.clone(),
                ));
            }
            (Pat::Lit(Lit::Char(*c)), Vec::new())
        }
        // A `..` array pattern is compiled directly, by length, and a nested
        // match is a decision tree over exact lengths; the two do not mix.
        Pattern::ArrayRest(_) => {
            return Err(Error::at(
                "an array pattern ending in `..` cannot share a `match` with a nested \
                 pattern — write the nested arms in their own `match`"
                    .to_string(),
                span.clone(),
            ))
        }
        Pattern::Array(_) | Pattern::ArrayNested(_) => {
            let ps = &pattern
                .array_elems()
                .expect("both array forms answer `array_elems`");
            let elem = seq_elem_ty(ty).ok_or_else(|| {
                Error::at(
                    format!(
                        "`[..]` pattern matches an array or a str, not {}",
                        tyname(ty)
                    ),
                    span.clone(),
                )
            })?;
            let mut subs = Vec::with_capacity(ps.len());
            let mut binders = Vec::new();
            for sub in ps {
                let (pat, bs) = bind_nested_inner(info, sub, &elem, span)?;
                subs.push(pat);
                binders.extend(bs);
            }
            // One synthetic constructor per length: rows of different
            // lengths never specialize against each other, which is exactly
            // how `[a]` and `[a, b]` tell two alternatives apart. An array
            // type's signature is `Open`, so the match still needs a `_`.
            (Pat::Ctor(patterns::array_ctor(ps.len()), subs), binders)
        }
        Pattern::Tuple(ps) => {
            let Some(elems) = info.tuple_elems(ty) else {
                return Err(Error::at(
                    format!(
                        "tuple pattern of {} matches a tuple, not {}",
                        ps.len(),
                        tyname(ty)
                    ),
                    span.clone(),
                ));
            };
            if elems.len() != ps.len() {
                return Err(Error::at(
                    format!(
                        "tuple pattern has {} elements, but {} has {}",
                        ps.len(),
                        tyname(ty),
                        elems.len()
                    ),
                    span.clone(),
                ));
            }
            let mut subs = Vec::with_capacity(ps.len());
            let mut binders = Vec::new();
            for (p, t) in ps.iter().zip(&elems) {
                let (sp, b) = bind_nested_inner(info, p, t, span)?;
                subs.push(sp);
                binders.extend(b);
            }
            (Pat::Ctor(patterns::TUPLE.to_string(), subs), binders)
        }
        Pattern::Ctor {
            name,
            bindings,
            ignore_payload,
        } => {
            let payload = pattern_case_payload(info, ty, name, span)?;
            // The matrix compares against the type's own case names, which
            // are bare; a pattern's may carry its `@Variant` qualification.
            let name = name.split('@').next().unwrap_or(name).to_string();
            if *ignore_payload {
                if payload.is_empty() {
                    return Err(Error::at(
                        format!(
                            "constructor {name:?} carries no payload, so \"..\" ignores \
                             nothing — write {name:?} instead"
                        ),
                        span.clone(),
                    ));
                }
                return Ok((
                    Pat::Ctor(name.clone(), vec![Pat::Wild; payload.len()]),
                    Vec::new(),
                ));
            }
            if bindings.len() != payload.len() {
                return Err(Error::at(
                    format!(
                        "constructor {name:?} binds {} value(s), but {} given",
                        payload.len(),
                        bindings.len()
                    ),
                    span.clone(),
                ));
            }
            let binders: Vec<(String, Type)> = bindings
                .iter()
                .zip(payload.iter())
                .filter(|(b, _)| b.as_str() != "_")
                .map(|(b, t)| (b.clone(), t.clone()))
                .collect();
            (
                Pat::Ctor(name.clone(), vec![Pat::Wild; payload.len()]),
                binders,
            )
        }
        Pattern::Nested { name, args } => {
            let payload = pattern_case_payload(info, ty, name, span)?;
            let name = name.split('@').next().unwrap_or(name).to_string();
            if args.len() != payload.len() {
                return Err(Error::at(
                    format!(
                        "constructor {name:?} carries {} value(s), but the pattern has {}",
                        payload.len(),
                        args.len()
                    ),
                    span.clone(),
                ));
            }
            let mut subs = Vec::with_capacity(args.len());
            let mut binders = Vec::new();
            for (p, t) in args.iter().zip(&payload) {
                let (sp, b) = bind_nested_inner(info, p, t, span)?;
                subs.push(sp);
                binders.extend(b);
            }
            (Pat::Ctor(name.clone(), subs), binders)
        }
    })
}

/// The payload types of case `name` of `ty` — `some`/`none` of an optional,
/// `ok`/`err` of a result, or a variant's case — or an error naming what
/// `ty` does offer.
pub(crate) fn pattern_case_payload(
    info: &dyn MatchInfo,
    ty: &Type,
    name: &str,
    span: &Span,
) -> Result<Vec<Type>, Error> {
    let bare: &str = name.split('@').next().unwrap_or(name);
    info.note_case(ty, bare);
    match ty {
        Type::Optional(inner) => match bare {
            "some" => Ok(vec![(**inner).clone()]),
            "none" => Ok(vec![]),
            other => Err(Error::at(
                format!("\"match\" on an optional expects \"some\"/\"none\", got {other:?}"),
                span.clone(),
            )),
        },
        Type::Result(ok, err) => match bare {
            "ok" if is_unit(ok) => Ok(vec![]),
            "ok" => Ok(vec![(**ok).clone()]),
            "err" => Ok(vec![(**err).clone()]),
            other => Err(Error::at(
                format!("\"match\" on a result expects \"ok\"/\"err\", got {other:?}"),
                span.clone(),
            )),
        },
        _ if info.cases_of(ty).is_some() => {
            let cases = info.cases_of(ty).expect("just checked");
            match cases.into_iter().find(|(c, _)| c.as_str() == bare) {
                Some((_, p)) => Ok(p),
                None => Err(Error::at(
                    format!("{} has no constructor {bare:?}", tyname(ty)),
                    span.clone(),
                )),
            }
        }
        other => Err(Error::at(
            format!(
                "constructor pattern {bare:?} needs an optional, result or variant, got {}",
                tyname(other)
            ),
            span.clone(),
        )),
    }
}

/// A match with nested patterns is exhaustive when no value escapes every
/// arm, and every arm must be reachable — both asked of the matrix
/// algorithm (`patterns::useful`), with the witness it hands back naming
/// what is missing.
pub(crate) fn check_nested_exhaustive(
    info: &dyn MatchInfo,
    rows: &[patterns::Row],
    st: &Type,
    arms: &[MatchArm],
    span: Span,
) -> Result<(), Error> {
    use patterns::Pat;
    let tys = [st.clone()];
    for (i, arm) in arms.iter().enumerate() {
        if patterns::useful(&rows[..i], &rows[i], &tys, info).is_none() {
            return Err(Error::at(
                "unreachable arm: every value it matches is matched by an arm above it".to_string(),
                arm.span.clone(),
            ));
        }
    }
    if let Some(witness) = patterns::useful(rows, &[Pat::Wild], &tys, info) {
        let what = patterns::render(&witness[0]);
        let message = if what == "_" {
            // An open domain (a literal column) with no catch-all: there is
            // no one value to name.
            "non-exhaustive match: not every value is matched — add a `_` or binder arm".to_string()
        } else {
            format!("non-exhaustive match: {what} is not matched")
        };
        return Err(Error::at(message, span));
    }
    Ok(())
}

/// Every constructor of the scrutinee's type must be matched exactly once.
pub(crate) fn check_match_exhaustive(
    info: &dyn MatchInfo,
    st: &Type,
    arms: &[MatchArm],
    span: Span,
) -> Result<(), Error> {
    // A `str` / array match compares by exact equality and is open-domain, so
    // it must end with a wildcard `_` arm (the default). Arms are tried
    // top-to-bottom, so the `_` must be last (anything after it is
    // unreachable), and the literal patterns must be distinct (a duplicate is
    // the only way an earlier arm makes a later one unreachable under exact
    // matching).
    if is_str_repr(st) || matches!(st, Type::Array(_) | Type::Primitive(Primitive::Char)) {
        let noun = if is_str_repr(st) {
            "a str"
        } else if matches!(st, Type::Primitive(Primitive::Char)) {
            "a char"
        } else {
            "an array"
        };
        for (idx, arm) in arms.iter().enumerate() {
            // The `_` arm must be last.
            if matches!(arm.pattern, Pattern::Wildcard) && idx != arms.len() - 1 {
                return Err(Error::at(
                    "the `_` arm must be last (arms after it are unreachable)".to_string(),
                    arm.span.clone(),
                ));
            }
            // A duplicate literal pattern is dead code. (`Pattern: Eq`, so this
            // compares string literals and array-literal element lists alike.)
            if !matches!(arm.pattern, Pattern::Wildcard)
                && arms[..idx].iter().any(|p| p.pattern == arm.pattern)
            {
                let what = match &arm.pattern {
                    Pattern::Str(lit) => format!("duplicate {lit:?} arm"),
                    Pattern::Char(c) => {
                        format!("duplicate {:?} arm", char::from(*c))
                    }
                    _ => "duplicate match arm".to_string(),
                };
                return Err(Error::at(what, arm.span.clone()));
            }
        }
        if !matches!(arms.last(), Some(a) if matches!(a.pattern, Pattern::Wildcard)) {
            return Err(Error::at(
                format!("non-exhaustive match on {noun}: add a `_` arm"),
                span.clone(),
            ));
        }
        return Ok(());
    }
    let required: Vec<String> = match st {
        Type::Optional(_) => vec!["some".into(), "none".into()],
        Type::Result(_, _) => vec!["ok".into(), "err".into()],
        // A concrete or (abstract) generic variant — every case must appear.
        _ if info.cases_of(st).is_some() => info
            .cases_of(st)
            .expect("just checked")
            .into_iter()
            .map(|(c, _)| c)
            .collect(),
        // A non-matchable scrutinee already errored in `match_arm_bindings`.
        _ => return Ok(()),
    };
    let mut seen: HashSet<&str> = HashSet::new();
    for arm in arms {
        // Non-constructor patterns already errored in `match_arm_bindings`.
        let Pattern::Ctor { name, .. } = &arm.pattern else {
            continue;
        };
        // Patterns may be variant-qualified (`Case@Variant`); compare against
        // the bare case names the variant declares.
        let bare = name.split('@').next().unwrap_or(name);
        if !seen.insert(bare) {
            return Err(Error::at(
                format!("duplicate \"{bare}\" arm"),
                arm.span.clone(),
            ));
        }
    }
    let missing: Vec<&str> = required
        .iter()
        .map(String::as_str)
        .filter(|c| !seen.contains(c))
        .collect();
    // A `_` arm covers whatever is left. It has to be last, since arms
    // after it can never run, and it has to have something left to cover,
    // or it is dead itself — which is the shape that would otherwise go
    // quiet the moment someone removes the last case it stood for.
    if let Some(i) = arms
        .iter()
        .position(|a| matches!(a.pattern, Pattern::Wildcard))
    {
        if i != arms.len() - 1 {
            return Err(Error::at(
                "the `_` arm must be last (arms after it are unreachable)".to_string(),
                arms[i].span.clone(),
            ));
        }
        if missing.is_empty() {
            return Err(Error::at(
                "unreachable `_` arm: every case is already matched above it".to_string(),
                arms[i].span.clone(),
            ));
        }
        return Ok(());
    }
    if !missing.is_empty() {
        return Err(Error::at(
            format!("non-exhaustive match: missing {}", missing.join(", ")),
            span.clone(),
        ));
    }
    Ok(())
}
