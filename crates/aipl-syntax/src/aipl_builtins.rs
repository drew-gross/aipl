//! The builtins *implemented in AIPL*, rather than expanded in Rust or
//! intrinsified in codegen. Which ones, and where their sources live, is
//! [`crate::builtin`]'s table; this is what the loader does with them.
//!
//! Most builtins stay codegen-generated because the generated IR beats what
//! compiled AIPL would produce; when it wouldn't, the implementation belongs in
//! real AIPL source, where it doubles as documentation and carries its own
//! `.test` and `.performance` blocks (see `builtin_all.aipl`).
//!
//! Such a source is an ordinary AIPL file. The loader loads it as one more file
//! of any program that reaches it — by importing it, or through [`companions`]
//! — so it is checked, linted and compiled exactly as the program's own files
//! are, may import other builtins, and may hold private helpers, structs and
//! constants. The one difference is its `pub fn`: written under the builtin's
//! user-facing spelling, so the file's tests read like user code, it is named
//! by the builtin's canonical `__builtin_*` name, which every call to the
//! builtin already carries. Its `.test` and `.performance` blocks run when the
//! file itself is checked (`aipl check`), not in every program that uses it.

use std::collections::HashSet;

use crate::ast::{Callee, ExprKind, Item};

/// The AIPL-implemented builtins a program reaching `present` may come to call
/// without naming them: what a pass after the checker rewrites a call into. A
/// fusion of two calls needs both (`map` then `find_if` becomes `map_find_if`);
/// a lowering needs the one it lowers (a reduction becomes `reduce`, a grid
/// operation one of the grid builtins, `extend` of an optional
/// `extend_optional`, an unsigned `int_parse` `int_parse_unsigned`).
///
/// The loader loads these with the program, since nothing it reads names them.
/// A pass that introduces a builtin must have a rule here: the monomorphizer
/// refuses a call to one the program does not hold.
pub fn companions(present: &HashSet<Callee>, mentions_grid: bool) -> HashSet<Callee> {
    use Callee as C;
    let has = |c: C| present.contains(&c);
    let reduction = present.iter().any(|c| c.reduction_step().is_some());
    let filters = has(C::Filter) || has(C::FilterMap);
    let rules: &[(bool, C)] = &[
        (has(C::Map) && has(C::FindIf), C::MapFindIf),
        (has(C::Map) && has(C::Join), C::MapJoin),
        (has(C::Join) && has(C::IsNonempty), C::JoinIsNonempty),
        (has(C::Join) && has(C::IsEmpty), C::JoinIsEmpty),
        (has(C::Split) && has(C::Map), C::SplitMap),
        (reduction, C::Reduce),
        // `xs.sort().first()` / `.last()` become `minimum` / `maximum`, which are
        // reductions.
        (has(C::Sort) && (has(C::First) || has(C::Last)), C::Reduce),
        (reduction && has(C::Map), C::MapReduce),
        (reduction && filters, C::FilterMapReduce),
        (filters, C::CountIf),
        // `xs.filter(p).first()` / `.last()` become a `find_if`.
        (has(C::Filter) && (has(C::First) || has(C::Last)), C::FindIf),
        // `xs.filter(p).is_nonempty()` / `.is_empty()` become an `any`.
        (filters && (has(C::IsEmpty) || has(C::IsNonempty)), C::Any),
        // `xs.count_if(p) <op> n` stops counting once the answer is settled; each
        // comparison, in either operand order, is its own builtin.
        (
            has(C::CountIf) && (has(C::LessThan) || has(C::GreaterThan)),
            C::CountIfIsLessThan,
        ),
        (
            has(C::CountIf) && (has(C::LessThanOrEqual) || has(C::GreaterThanOrEqual)),
            C::CountIfIsAtMost,
        ),
        (
            has(C::CountIf) && (has(C::LessThan) || has(C::GreaterThan)),
            C::CountIfIsGreaterThan,
        ),
        (
            has(C::CountIf) && (has(C::LessThanOrEqual) || has(C::GreaterThanOrEqual)),
            C::CountIfIsAtLeast,
        ),
        (has(C::CountIf) && has(C::Equal), C::CountIfIsEqual),
        (has(C::CountIf) && has(C::NotEqual), C::CountIfIsNotEqual),
        (has(C::Count), C::CountIsLessThan),
        (has(C::Count), C::CountIsAtMost),
        (has(C::Count), C::CountIsGreaterThan),
        (has(C::Count), C::CountIsAtLeast),
        (has(C::Count), C::CountIsEqual),
        (has(C::Count), C::CountIsNotEqual),
        (has(C::Extend), C::ExtendOptional),
        (has(C::IntParse), C::IntParseUnsigned),
        (mentions_grid, C::GridFrom),
        (mentions_grid, C::GridGet),
        (mentions_grid, C::GridTrySet),
        (mentions_grid, C::GridIsInBoundsOf),
    ];
    rules
        .iter()
        .filter(|(applies, _)| *applies)
        .map(|(_, c)| c.clone())
        .collect()
}

/// Whether `items` name a grid type anywhere a type is written: a signature, a
/// struct or variant declaration, or a `let` annotation — the places that make a
/// grid operation of an index, a literal or a `try_set`. Errs toward yes.
pub fn mentions_grid(items: &[Item]) -> bool {
    let named = |written: &dyn std::fmt::Debug| format!("{written:?}").contains("Grid");
    let mut found = items.iter().any(|item| match item {
        Item::Fn(f) => named(&f.sig),
        Item::Struct(_) | Item::Variant(_) => named(item),
        _ => false,
    });
    crate::each_item_expr(items, &mut |e| {
        if let ExprKind::Let(_, Some(ty), ..) | ExprKind::LetMut(_, Some(ty), ..) = &e.kind {
            found = found || named(ty);
        }
    });
    found
}
