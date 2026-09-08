use super::{field_reads, has_tuple_field, mentions_any, shared_fields, uses_of, Uses};
use crate::ast::{Function, Item, Param, Program, Type};
use crate::Error;

/// `fn f(p: Point) { .. p.x .. p.y .. }` where **every** mention of `p` is a
/// field read — destructure the parameter instead: `fn f(Point { x, y })`.
///
/// The same argument [`super::destructure_binding`] makes about a `let`, one
/// step earlier. A parameter that is only ever taken apart is a pattern written
/// the long way, and the signature is where a reader looks first: `fn f(Point {
/// x, y })` says what the function needs, where `fn f(p: Point)` says only that
/// it needs a `Point` and leaves the body to be scanned for how much of it.
///
/// The parameter's type is right there in the signature, so unlike the binding
/// lint this one can name the struct in its advice rather than leaving a
/// placeholder.
///
/// **What is left alone**, beyond everything the shared conditions already
/// exclude (a bare mention, a name the pattern would collide with, a field read
/// off another value, a tuple):
///
/// - **`self`.** Method-call syntax is defined by the receiver being named
///   `self`, so a pattern there would change how the function is called.
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
        || p.variadic
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
    // that survives `aipl fmt` — and the closest line to the signature that a
    // parameter can be blamed on, `ast::Param` carrying no span of its own.
    // `fn_body_type_stutter` blames the same place for the same reason.
    hits.push(Error::at(
        format!(
            "only the fields of parameter \"{}\" are read — destructure it instead: \
             \"{struct_name} {{ {} }}\" in place of \"{}: {struct_name}\" \
             (or append #[allow] to this line to keep it)",
            p.name,
            fields.join(", "),
            p.name,
        ),
        f.body.span.clone(),
    ));
}
