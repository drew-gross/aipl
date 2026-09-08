use super::{field_reads, has_tuple_field, mentions_any, shared_fields, uses_of, Uses};
use crate::ast::{Expr, ExprKind, Item, Program, Type};
use crate::{each_subexpr, Error};
use std::collections::HashMap;

/// `let p = value; ... p.a ... p.b ...` where **every** mention of `p` is a
/// field read — say so with a pattern instead: `let T { a, b } = value;`.
///
/// The binding then names what it actually holds. A reader of
/// `let P { line, col } = pos_of(i);` knows the two fields are the whole story;
/// with `let p = pos_of(i);` they have to scan the rest of the block to find out
/// whether `p` is also passed somewhere, compared, or returned.
///
/// One bare mention is enough to stay quiet, because a pattern cannot reproduce
/// it — the value itself is gone, only its fields survive. That covers the
/// obvious escapes (`f(p)`, `p == q`, a trailing `p`) without special-casing
/// any of them.
///
/// Only immutable `let` is flagged. A `mut` binding is a different question —
/// whether it should be `mut` at all — and answering it here would advise a
/// rewrite that changes more than the spelling.
///
/// **A pattern binds one name per field, so it is only available when each of
/// those names would mean one thing in the function.** Two conditions take the
/// lint out, and between them they are most of what it would otherwise flag:
///
/// - **The name is already spoken for.** `let b = w.bump();` reading `b.text`
///   and `b.w` cannot become `let Bump { text, w } = w.bump();` where a `w` is
///   in scope: the new binding shadows it and every later mention of the old one
///   silently changes meaning. This is the shape the lint met most often — a
///   cursor threaded through a `w` field, read alongside its own name.
///
///   The binding's *own* name does not count, even when a field shares it
///   (`let segs = ..` reading `segs.segs`). That binding is the one going away,
///   and the `field_reads == mentions` test above has already established that
///   every mention of it is the base of a field read the pattern replaces —
///   so there is provably nothing left for the new name to shadow.
///
/// - **The field is read off more than one value.** Three `let`s binding the
///   same struct in one block are told apart by their *names*
///   (`plain.cleaned`, `empty.cleaned`); destructuring them all binds `cleaned`
///   three times, shadowing in sequence, and the distinction the names carried
///   is gone. So a field read off two different bases anywhere in the function
///   disqualifies every binding it belongs to — which is why this is a
///   whole-function pass rather than a per-expression one. Checking only the
///   binding's own body would see the *last* of such a group as unambiguous and
///   flag it alone.
///
///   Reads inside the binding's **own value** are the exception, and they are
///   not a rarity: threading a cursor writes `let segs = head.w.next()?` and
///   then reads `segs.w`, so `w` has two bases and neither is ambiguous. The
///   value is evaluated before the pattern binds anything, so no name the
///   pattern introduces can be confused with a read in it — by the time `w`
///   means the field, `head.w` has already been consumed.
pub(super) fn destructure_binding(program: &Program, hits: &mut Vec<Error>) {
    for item in &program.items {
        let Item::Fn(f) = item else {
            continue;
        };
        for body in [Some(&f.body), f.test_body.as_ref()].into_iter().flatten() {
            let all = field_reads(body);
            each_subexpr(body, &mut |e| one_binding(e, &all, hits));
        }
    }
}

fn one_binding(e: &Expr, all: &HashMap<(String, String), usize>, hits: &mut Vec<Error>) {
    let ExprKind::Let(name, ann, value, body) = &e.kind else {
        return;
    };
    // The parser's own desugarings bind temporaries whose only use *is* a field
    // read — a `let T { .. }` pattern lowers to exactly that, and so does a
    // destructured parameter, so linting them would advise rewriting the
    // rewrite, on a name the user never wrote and cannot put an `#[allow]` on.
    if name.starts_with("__") {
        return;
    }
    // ...nor the field bindings such a desugaring introduces, whose value is a
    // read off that temporary. Those names came from a pattern the user already
    // wrote: advising a pattern *inside* a pattern is the lint chasing its own
    // output, and since it runs before type checking it cannot tell that the
    // field it would take apart is a `Range` rather than a struct — the advice
    // for `let FmtTok { span, .. } = ..` reading only `span.end` was to
    // destructure the range.
    if matches!(&value.kind, ExprKind::Field(base, _)
        if matches!(&base.kind, ExprKind::Ident(b) if b.starts_with("__")))
    {
        return;
    }
    let Uses {
        mentions,
        field_reads,
        fields,
        shadowed,
    } = uses_of(body, name);
    // A rebinding of the same name inside the body splits the mentions between
    // two different values, and this walk cannot tell which is which. Rather
    // than track scopes for a case that is rare and confusing to read anyway,
    // say nothing.
    if shadowed || field_reads == 0 || field_reads != mentions {
        return;
    }
    // Is one of these field names also read off some *other* value that is still
    // around once the pattern binds it? Then the bare name would mean two things
    // and the binding names are what tell them apart today.
    let shared = shared_fields(all, Some(value));
    if fields.iter().any(|f| shared.contains(f)) || has_tuple_field(&fields) {
        return;
    }
    // The binding's own name is exempt in the body — it is the name going away,
    // and every mention of it there is the base of a read the pattern replaces
    // (`field_reads == mentions`, above). In the *value* nothing is exempt: that
    // is evaluated before the pattern binds, so a name it mentions is an outer
    // one the pattern would shadow for the rest of the block.
    if mentions_any(body, &fields, Some(name)) || mentions_any(value, &fields, None) {
        return;
    }
    // Taking apart a value built on the same line is not an improvement: the
    // pattern restates the type name that literal just wrote and re-lists the
    // fields it just set, so `let p = Point { x: 1, y: 2 };` reading `p.x`/`p.y`
    // would become `let Point { x, y } = Point { x: 1, y: 2 };`. That is the same
    // objection `fn_body_type_stutter` makes about naming a type twice.
    if matches!(&value.kind, ExprKind::Construct(..)) {
        return;
    }
    // The struct is named only where the source already names it — an
    // annotation. A call's result type is not knowable here (lints run before
    // type checking), so the advice leaves a placeholder rather than guessing.
    let ty = match ann {
        Some(Type::Named(t)) => t.clone(),
        _ => "<Type>".to_string(),
    };
    hits.push(Error::at(
        format!(
            "only the fields of \"{name}\" are read — destructure the binding instead: \
             \"let {ty} {{ {} }} = ...\" (or append #[allow] to this line to keep it)",
            fields.join(", ")
        ),
        value.span.clone(),
    ));
}
