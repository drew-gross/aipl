use super::{field_reads, has_tuple_field, mentions_any, shared_fields, uses_of, Uses};
use crate::ast::{Expr, ExprKind, Function, Item, Param, Program, Type};
use crate::Error;

/// `fn f(p: Point) { .. p.x .. p.y .. }` where **every** mention of `p` is a
/// field read — destructure the parameter instead: `fn f(Point { x, y })`.
///
/// The same argument `destructure_binding` (in `destructure_binding.aipl`) makes
/// about a `let`, one step earlier. A parameter that is only ever taken apart is a pattern written
/// the long way, and the signature is where a reader looks first: `fn f(Point {
/// x, y })` says what the function needs, where `fn f(p: Point)` says only that
/// it needs a `Point` and leaves the body to be scanned for how much of it.
///
/// The parameter's type is right there in the signature, so unlike the binding
/// lint this one can name the struct in its advice rather than leaving a
/// placeholder.
///
/// A parameter whose only use is a `let T { a, b } = p;` that opens the body is
/// the same pattern written one step late, and is flagged too: the pattern
/// moves into the signature unchanged.
///
/// **What is left alone**, beyond everything the shared conditions already
/// exclude (a bare mention, a name the pattern would collide with, a field read
/// off another value, a tuple):
///
/// - **`self`.** Method-call syntax is defined by the receiver being named
///   `self`, so its pattern is the named form `self: Span { start, end }`, not
///   the `Span { start, end }` this lint's advice spells.
/// - **A keyword parameter.** Its name *is* the call-site spelling (`f(sep =
///   ", ")`), so destructuring it would break every caller.
/// - **`mut` and variadic parameters.** A `mut` receiver is written through
///   rather than read, and a variadic parameter's type is a sequence.
/// - **Anything but a plain named type.** A generic instance is a template
///   until monomorphization and a parameter has no value to infer arguments
///   from, so `Box { v }` has nothing to bind; arrays, optionals and primitives
///   have no fields to name.
pub(super) fn destructure_param(program: &Program, hits: &mut Vec<Error>) {
    for item in &program.items {
        let Item::Fn(f) = item else {
            continue;
        };
        let all = field_reads(&f.body);
        for p in &f.sig.params {
            one_param(f, p, &all, hits);
        }
    }
}

fn one_param(
    f: &Function,
    p: &Param,
    all: &std::collections::HashMap<(String, String), usize>,
    hits: &mut Vec<Error>,
) {
    // A parameter the parser introduced for a pattern the user already wrote.
    if p.name == "self"
        || p.mutable
        || p.arity.is_variadic()
        || p.default.is_some()
        || p.name.starts_with("__")
    {
        return;
    }
    let Type::Named(struct_name) = &p.ty else {
        return;
    };
    let Uses {
        mentions,
        field_reads,
        fields,
        shadowed,
    } = uses_of(&f.body, &p.name);
    if !shadowed && field_reads == 0 && mentions == 1 {
        if let Some((value_span, fields)) = opening_destructure(&f.body, p, struct_name) {
            hits.push(Error::at(
                format!(
                    "parameter \"{}\" is only taken apart by a destructuring `let` — destructure \
                     it in the signature instead: \"{struct_name} {{ {} }}\" in place of \
                     \"{}: {struct_name}\"",
                    p.name,
                    fields.join(", "),
                    p.name,
                ),
                value_span,
            ));
            return;
        }
    }
    if shadowed || field_reads == 0 || field_reads != mentions {
        return;
    }
    // There is no right-hand side to discount here the way a `let` has one: a
    // parameter is bound before the body runs, so every read in the function is
    // one the pattern's names would have to live alongside.
    let shared = shared_fields(all, None);
    if fields.iter().any(|x| shared.contains(x)) || has_tuple_field(&fields) {
        return;
    }
    // The parameter's own name is exempt, as the binding's is: it is the name
    // going away, and every mention of it is the base of a read the pattern
    // replaces.
    if mentions_any(&f.body, &fields, Some(&p.name)) {
        return;
    }
    // The body's span starts at the `{` that opens it, which is a single line
    // that survives the formatter — and the closest line to the signature that a
    // parameter can be blamed on, `ast::Param` carrying no span of its own.
    // `fn_body_type_stutter` blames the same place for the same reason.
    hits.push(Error::at(
        format!(
            "only the fields of parameter \"{}\" are read — destructure it instead: \
             \"{struct_name} {{ {} }}\" in place of \"{}: {struct_name}\"",
            p.name,
            fields.join(", "),
            p.name,
        ),
        f.body.span.clone(),
    ));
}

/// The span of the value and the field names of a `let T { a, b } = <p>;` that
/// is the first statement of `body`, when it binds each field to a name of its
/// own spelling and names the parameter's type.
///
/// The parser lowers the statement to a `__spat$` temporary holding the value,
/// then one `let` per field reading it. A field bound under another name
/// (`T { a: x }`) is not recognised: the signature's pattern would have to
/// spell the rename too.
fn opening_destructure(
    body: &Expr,
    p: &Param,
    struct_name: &str,
) -> Option<(crate::Span, Vec<String>)> {
    let ExprKind::Let(temporary, Some(Type::Named(named)), value, rest) = &body.kind else {
        return None;
    };
    if !temporary.starts_with(crate::DESTRUCTURE_BASE_PREFIX) || named != struct_name {
        return None;
    }
    if !matches!(&value.kind, ExprKind::Ident(n) if *n == p.name) {
        return None;
    }
    let mut fields = Vec::new();
    let mut next = &**rest;
    while let ExprKind::Let(name, None, read, inner) = &next.kind {
        let ExprKind::Field(base, field) = &read.kind else {
            break;
        };
        if !matches!(&base.kind, ExprKind::Ident(b) if b == temporary) || name != field {
            break;
        }
        fields.push(field.clone());
        next = inner;
    }
    // Every read of the temporary must have been one of the plain bindings above.
    let reads = uses_of(rest, temporary).mentions;
    (!fields.is_empty() && reads == fields.len()).then(|| (value.span.clone(), fields))
}
