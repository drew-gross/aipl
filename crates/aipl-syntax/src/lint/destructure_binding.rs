use crate::ast::{Expr, ExprKind, Item, Program, Type};
use crate::{each_subexpr, Error};
use std::collections::{HashMap, HashSet};

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
/// - **The field is read off more than one value.** Three `let`s binding the
///   same struct in one block are told apart by their *names*
///   (`plain.cleaned`, `empty.cleaned`); destructuring them all binds `cleaned`
///   three times, shadowing in sequence, and the distinction the names carried
///   is gone. So a field read off two different bases anywhere in the function
///   disqualifies every binding it belongs to — which is why this is a
///   whole-function pass rather than a per-expression one. Checking only the
///   binding's own body would see the *last* of such a group as unambiguous and
///   flag it alone.
pub(super) fn destructure_binding(program: &Program, hits: &mut Vec<Error>) {
    for item in &program.items {
        let Item::Fn(f) = item else {
            continue;
        };
        for body in [Some(&f.body), f.test_body.as_ref()].into_iter().flatten() {
            let shared = fields_read_off_many(body);
            each_subexpr(body, &mut |e| one_binding(e, &shared, hits));
        }
    }
}

/// Field names read off more than one base anywhere in `body` — see
/// [`destructure_binding`]'s second condition.
fn fields_read_off_many(body: &Expr) -> HashSet<String> {
    let mut bases: HashMap<String, HashSet<String>> = HashMap::new();
    each_subexpr(body, &mut |x| {
        if let ExprKind::Field(base, f) = &x.kind {
            if let ExprKind::Ident(b) = &base.kind {
                bases.entry(f.clone()).or_default().insert(b.clone());
            }
        }
    });
    bases
        .into_iter()
        .filter(|(_, b)| b.len() > 1)
        .map(|(f, _)| f)
        .collect()
}

fn one_binding(e: &Expr, shared: &HashSet<String>, hits: &mut Vec<Error>) {
    let ExprKind::Let(name, ann, value, body) = &e.kind else {
        return;
    };
    // The parser's own desugarings bind temporaries whose only use *is* a field
    // read — a `let T { .. }` pattern lowers to exactly that, so linting them
    // would advise rewriting the rewrite, on a name the user never wrote and
    // cannot put an `#[allow]` on.
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
    let (mut mentions, mut field_reads) = (0usize, 0usize);
    let mut fields: Vec<String> = Vec::new();
    let mut shadowed = false;
    each_subexpr(body, &mut |x| {
        match &x.kind {
            ExprKind::Ident(n) if n == name => mentions += 1,
            ExprKind::Field(base, f) if is_name(base, name) => {
                field_reads += 1;
                if !fields.iter().any(|k| k == f) {
                    fields.push(f.clone());
                }
            }
            _ => {}
        }
        shadowed |= rebinds(x, name);
    });
    // A rebinding of the same name inside the body splits the mentions between
    // two different values, and this walk cannot tell which is which. Rather
    // than track scopes for a case that is rare and confusing to read anyway,
    // say nothing.
    if shadowed || field_reads == 0 || field_reads != mentions {
        return;
    }
    // Would any of the names the pattern introduces collide with one already
    // spoken for? A bare mention anywhere in the value or body is enough to say
    // yes: this walk does not track scopes, and a name that is used is a name
    // the pattern cannot quietly take over.
    if fields.iter().any(|f| shared.contains(f)) {
        return;
    }
    // A tuple is read by position (`t._0`), and no struct pattern can take one
    // apart — there is no type name to write. `let (a, b) = t;` is the form for
    // it, but it has to name *every* position, and this pass sees only the ones
    // that were read. So tuples are left alone rather than advised into a
    // pattern that would not compile.
    if fields.iter().any(|f| {
        f.strip_prefix('_')
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    }) {
        return;
    }
    let mut collides = false;
    let mut mentioned = |x: &Expr| {
        if let ExprKind::Ident(n) = &x.kind {
            collides |= fields.iter().any(|f| f == n);
        }
    };
    each_subexpr(value, &mut mentioned);
    each_subexpr(body, &mut mentioned);
    if collides {
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

fn is_name(e: &Expr, name: &str) -> bool {
    matches!(&e.kind, ExprKind::Ident(n) if n == name)
}

/// Whether `e` introduces a binding called `name`, in any of the positions that
/// can: the two `let` forms, a lambda parameter, a loop variable, and a match
/// arm's payload binders.
fn rebinds(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Let(n, _, _, _) | ExprKind::LetMut(n, _, _, _) | ExprKind::For(n, _, _) => {
            n == name
        }
        ExprKind::Lambda(params, _) => params.iter().any(|p| p.name == name),
        ExprKind::Match(_, arms) => arms
            .iter()
            .any(|a| a.pattern.bindings().iter().any(|b| b == name)),
        _ => false,
    }
}
