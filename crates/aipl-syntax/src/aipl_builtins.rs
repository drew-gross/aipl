//! The builtins *implemented in AIPL*, rather than expanded in Rust or
//! intrinsified in codegen, and where their sources live.
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
use std::path::PathBuf;

use crate::ast::{Callee, ExprKind, Item};

/// The directory the sources live in, `aipl-mono/src/`: resolved at compile
/// time but *read at run time*. `include_str!` would make every edit to one of
/// these files a source change to this crate, rebuilding it, everything
/// downstream and every test binary — paid on a plain edit, and again on each
/// one the test harness's `fill_expected` refreshes.
const SOURCE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../aipl-mono/src");

const SOURCES: &[(Callee, &str)] = &[
    (Callee::All, "builtin_all.aipl"),
    (Callee::Range, "builtin_range.aipl"),
    (Callee::SortBy, "builtin_sort_by.aipl"),
    (Callee::Any, "builtin_any.aipl"),
    (Callee::LeftFold, "builtin_left_fold.aipl"),
    (Callee::RightFold, "builtin_right_fold.aipl"),
    (Callee::OptLeftFold, "builtin_opt_left_fold.aipl"),
    (Callee::OptRightFold, "builtin_opt_right_fold.aipl"),
    (Callee::Count, "builtin_count.aipl"),
    (Callee::CountWhile, "builtin_count_while.aipl"),
    (Callee::CountIf, "builtin_count_if.aipl"),
    (Callee::FindIf, "builtin_find_if.aipl"),
    (Callee::MapFindIf, "builtin_map_find_if.aipl"),
    (Callee::MapJoin, "builtin_map_join.aipl"),
    (Callee::JoinIsNonempty, "builtin_join_is_nonempty.aipl"),
    (Callee::JoinIsEmpty, "builtin_join_is_empty.aipl"),
    (Callee::Reduce, "builtin_reduce.aipl"),
    (Callee::Repeat, "builtin_repeat.aipl"),
    (Callee::MapReduce, "builtin_map_reduce.aipl"),
    (Callee::FilterMapReduce, "builtin_filter_map_reduce.aipl"),
    (Callee::SplitMap, "builtin_split_map.aipl"),
    (Callee::FindMap, "builtin_find_map.aipl"),
    (Callee::ReverseFindMap, "builtin_reverse_find_map.aipl"),
    (Callee::FindIndex, "builtin_find_index.aipl"),
    (Callee::UnionAll, "builtin_union_all.aipl"),
    (Callee::SetMap, "builtin_set_map.aipl"),
    (Callee::ExtendOptional, "builtin_extend_optional.aipl"),
    (Callee::IsAllWhitespace, "builtin_is_all_whitespace.aipl"),
    (Callee::IsErrAnd, "builtin_is_err_and.aipl"),
    (Callee::IsSomeAnd, "builtin_is_some_and.aipl"),
    (Callee::Intersperse, "builtin_intersperse.aipl"),
    (Callee::TupleWindows, "builtin_tuple_windows.aipl"),
    (Callee::CountIsLessThan, "builtin_count_is_less_than.aipl"),
    (Callee::CountIsAtMost, "builtin_count_is_at_most.aipl"),
    (
        Callee::CountIsGreaterThan,
        "builtin_count_is_greater_than.aipl",
    ),
    (Callee::CountIsAtLeast, "builtin_count_is_at_least.aipl"),
    (Callee::CountIsEqual, "builtin_count_is_equal.aipl"),
    (Callee::CountIsNotEqual, "builtin_count_is_not_equal.aipl"),
    (Callee::First, "builtin_first.aipl"),
    (Callee::GridGet, "builtin_grid_get.aipl"),
    (Callee::GridFrom, "builtin_grid_from.aipl"),
    (Callee::GridTrySet, "builtin_grid_try_set.aipl"),
    (Callee::Width, "builtin_width.aipl"),
    (Callee::SeededWithInt, "builtin_seeded_with_int.aipl"),
    (Callee::SeededWithClock, "builtin_seeded_with_clock.aipl"),
    (Callee::Height, "builtin_height.aipl"),
    (Callee::IsInBoundsOf, "builtin_is_in_bounds_of.aipl"),
    (
        Callee::GridIsInBoundsOf,
        "builtin_grid_is_in_bounds_of.aipl",
    ),
    (Callee::Last, "builtin_last.aipl"),
    (Callee::NonemptyFirst, "builtin_nonempty_first.aipl"),
    (Callee::NonemptyLast, "builtin_nonempty_last.aipl"),
    (Callee::EnsureNonempty, "builtin_ensure_nonempty.aipl"),
    (Callee::DropFirst, "builtin_drop_first.aipl"),
    (Callee::DropLast, "builtin_drop_last.aipl"),
    (Callee::DropN, "builtin_drop_n.aipl"),
    (Callee::DropLastN, "builtin_drop_last_n.aipl"),
    (Callee::MapErr, "builtin_map_err.aipl"),
    (Callee::MapOk, "builtin_map_ok.aipl"),
    (Callee::TryMap, "builtin_try_map.aipl"),
    (Callee::IntParse, "builtin_int_parse.aipl"),
    (Callee::IntParseUnsigned, "builtin_int_parse_unsigned.aipl"),
    (Callee::TrimWhile, "builtin_trim_while.aipl"),
    (Callee::ValueOr, "builtin_value_or.aipl"),
    (Callee::ValueOrErr, "builtin_value_or_err.aipl"),
    (Callee::InlineStr, "builtin_inline_str.aipl"),
    (Callee::Measure, "builtin_measure.aipl"),
];

/// The source file of the AIPL-implemented builtin `callee`, or `None` for any
/// other callee.
pub fn source_path(callee: &Callee) -> Option<PathBuf> {
    SOURCES
        .iter()
        .find(|(c, _)| c == callee)
        .map(|(_, file)| PathBuf::from(SOURCE_DIR).join(file))
}

/// The builtin whose source is the file at `path`, judged by its file name.
pub fn implemented_at(path: &std::path::Path) -> Option<Callee> {
    let file = path.file_name()?.to_str()?;
    SOURCES
        .iter()
        .find(|(_, source)| *source == file)
        .map(|(c, _)| c.clone())
}

/// Whether `callee` is implemented in AIPL.
pub fn is_aipl_implemented(callee: &Callee) -> bool {
    SOURCES.iter().any(|(c, _)| c == callee)
}

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
        (reduction && has(C::Map), C::MapReduce),
        (reduction && filters, C::FilterMapReduce),
        (filters, C::CountIf),
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
