use crate::ast::{Expr, ExprKind, Program};
use crate::Error;

use super::{imported_as, mentions, quote, root_name};

/// What [`set_spread_push`] needs from the file's imports: the local names of
/// the two in-place appends it advises, or `None` for one the file never
/// imported.
///
/// The shape it flags is all language syntax — an array literal with a spread
/// in it needs no import at all — so unlike
/// [`push_array_literal`](super::push_array_literal()) this lint fires whether
/// or not the names are here. They are only for the advice, which says to
/// import what it uses when it is missing; otherwise following the advice would
/// just trade this error for an unknown-name one.
pub(super) struct AppendNames {
    push: Option<String>,
    extend: Option<String>,
}

pub(super) fn append_names(program: &Program) -> AppendNames {
    AppendNames {
        push: imported_as(program, "push"),
        extend: imported_as(program, "extend"),
    }
}

/// `set out = [..out, x];` — a binding rebuilt from itself only to append to
/// it. `set out.push(x);` is the append spelled as one: it names the element
/// added, and the array's other contents never come up.
///
/// The rebuild spelling says "build the longer array, then store it", and that
/// is what it costs: a fresh array of the combined length on every append, with
/// `out`'s old buffer released afterwards. `push` grows `out`'s own buffer in
/// place when nothing else holds it — and when something does, it copies, so
/// the two spellings agree on what any other binding sees. It is the array
/// sibling of [`set_spread_field`](super::set_spread_field()), which says the
/// same thing about a struct rebuilt from itself to replace a field, and of
/// [`extend_longhand`](super::extend_longhand()), which says it about a `+++`.
///
/// A spread in the tail is an append too, of a whole array rather than one
/// element, so it is advised as `extend` — `set out = [..out, ..xs];` is
/// `set out.extend(xs);`. A field path works the same way as a bare name
/// (`set self.hits = [..self.hits, x]` is `set self.hits.push(x)`), which is
/// the whole set of targets: `push` writes the grown array back into the place
/// it was given, and only a name or a field path off one is a place.
///
/// There is no blind spot about the element type. The two spellings store the
/// same things — `is_storable_elem` in `aipl-codegen` answers for both — right
/// down to the untyped-empty `__none__` a generic instantiated at `[]` carries,
/// which `push` used to refuse and the spread did not.
///
/// Two shapes are left alone. The spread must be the **first** element and must
/// be the target itself — `[x, ..out]` prepends and `[..other, x]` starts from
/// a different array, and neither is something `push` can spell. And with more
/// than one appended element the rewrite is one call per element, so it is
/// offered only when no element reads the target: `set out = [..out, out.len(),
/// out.len()];` appends the *same* index twice, where the two pushes would see
/// the array grow between them. A single element is always safe — it is
/// computed before the append either way.
pub(super) fn set_spread_push(e: &Expr, src: &str, names: &AppendNames, hits: &mut Vec<Error>) {
    let ExprKind::Assign(lhs, value, _) = &e.kind else {
        return;
    };
    let ExprKind::ArrayLit(elems) = &value.kind else {
        return;
    };
    let Some(root) = root_name(lhs) else {
        return;
    };
    let Some((spread, tail)) = elems.split_first() else {
        return;
    };
    let ExprKind::Spread(base) = &spread.kind else {
        return;
    };
    if base.kind != lhs.kind || tail.is_empty() {
        return;
    }
    if tail.len() > 1 && tail.iter().any(|x| mentions(x, root)) {
        return;
    }
    // Name the import for each append the advice actually uses, and only
    // those: a rewrite that is all pushes has nothing to say about `extend`.
    let mut missing: Vec<&str> = Vec::new();
    let (push, extend) = (local(&names.push, "push"), local(&names.extend, "extend"));
    let target = &src[lhs.span.clone()];
    let calls = tail
        .iter()
        .map(|x| match &x.kind {
            ExprKind::Spread(inner) => {
                note_missing(&names.extend, "extend", &mut missing);
                format!("set {target}.{extend}({});", quote(inner, src, "<array>"))
            }
            _ => {
                note_missing(&names.push, "push", &mut missing);
                format!("set {target}.{push}({});", quote(x, src, "<element>"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let import = match missing.as_slice() {
        [] => String::new(),
        [one] => format!(", importing `{one}` from builtins"),
        names => format!(", importing `{}` from builtins", names.join("` and `")),
    };
    let advice = if tail.len() == 1 {
        "append in place"
    } else {
        "append each in place"
    };
    // Point at the spread, not the whole `set`. `#[allow]` is line-scoped (see
    // [`check`](super::check())), and the spread is both where the shape starts
    // and a single line however the literal around it is broken.
    hits.push(Error::at(
        format!(
            "this \"set\" rebuilds {target:?} from itself only to append to it — {advice}: \
             \"{calls}\"{import} (or append #[allow] to this line to keep it)"
        ),
        spread.span.clone(),
    ));
}

/// The local spelling of a builtin, falling back to its exported name for a
/// file that never imported it — which is then both what the advice writes and
/// what it names as the missing import.
fn local<'a>(name: &'a Option<String>, builtin: &'a str) -> &'a str {
    name.as_deref().unwrap_or(builtin)
}

/// Record `builtin` as needing an import, once, when this file has no name for
/// it.
fn note_missing<'a>(name: &Option<String>, builtin: &'a str, missing: &mut Vec<&'a str>) {
    if name.is_none() && !missing.contains(&builtin) {
        missing.push(builtin);
    }
}
