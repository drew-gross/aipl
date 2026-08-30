use crate::ast::{ExprKind, Item, Program, Type};
use crate::Error;

/// `fn f() -> T { T { .. } }` — a function whose whole body is a literal of
/// its own declared return type, naming that type twice. The body-shorthand
/// spelling drops the repetition: `fn f() -> T { .. }`.
///
/// Both spellings parse to the same `Construct`, so which one was written is
/// recovered from the source: the long form's span starts at the type name
/// (`Atom::Construct` joins from `name_span`), the shorthand's at the first
/// field name. Requiring a `{` after that name is what keeps a field that
/// happens to share the type's name from reading as the long form — the
/// same source-scan idiom `field_init_shorthand` uses.
///
/// Only a body that is *exactly* the literal qualifies: a block with any
/// statement in it parses to a `Let`/`Seq` wrapping the literal, and the
/// shorthand replaces the whole body, so those are left alone.
pub(super) fn fn_body_type_stutter(program: &Program, src: &str, hits: &mut Vec<Error>) {
    for item in &program.items {
        let Item::Fn(f) = item else {
            continue;
        };
        // The name the return type offers the literal: a generic return
        // contributes its base, since `Pair<i64>` is built as `Pair { .. }`.
        let ret = match &f.sig.return_ty {
            Some(Type::Named(n)) => n,
            Some(Type::Generic(base, _)) => base,
            _ => continue,
        };
        let ExprKind::Construct(name, inits) = &f.body.kind else {
            continue;
        };
        // A construct the parser synthesized from other syntax (a range is
        // `__builtin_Span { .. }`) is not a literal anyone wrote, and its
        // name can't be spelled, so it has no shorter form.
        if name != ret || name.starts_with("__builtin_") || inits.is_empty() {
            continue;
        }
        // `{}` has no shorthand (it would read as an empty block), and the
        // source scan below needs the body to start at the type name.
        let Some(rest) = src
            .get(f.body.span.start..)
            .and_then(|s| s.strip_prefix(name.as_str()))
        else {
            continue;
        };
        if !rest.trim_start().starts_with('{') {
            continue; // already the shorthand
        }
        // A lone *shorthand* field is the one case whose replacement isn't
        // obvious: `{ x }` reads back as a block whose value is `x`, so the
        // trailing comma is load-bearing and the message spells it out.
        let lone_shorthand =
            inits.len() == 1 && !src[..inits[0].value.span.start].trim_end().ends_with(':');
        let advice = if lone_shorthand {
            format!(
                "write the field directly as the body, \"{{ {}, }}\" — the trailing comma is \
                 what keeps it a struct literal rather than a block",
                inits[0].name
            )
        } else {
            "write the fields directly as the body".to_string()
        };
        hits.push(Error::at(
            format!(
                "fn {:?} returns {name:?} and its body is a {name:?} literal, naming the type \
                 twice — {advice} (or append #[allow] to this line to keep it)",
                f.name
            ),
            f.body.span.clone(),
        ));
    }
}
