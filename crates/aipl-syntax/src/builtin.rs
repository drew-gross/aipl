//! Everything known about each importable builtin, in one place: which
//! callee it is, and where its signature and documentation come from.
//!
//! A builtin is one of three kinds, and a [`Definition`] says which:
//!
//! - **Written in AIPL.** Its source is an ordinary AIPL file (see
//!   [`crate::aipl_builtins`]), which holds the `pub fn` and the `# ..` block
//!   that are its signature and documentation. A tool reads them from there.
//! - **Declared.** The compiler expands or generates it, and the checker types
//!   calls to it by a declaration in [`crate::BUILTIN_SIGNATURES`] named
//!   `__builtin_<name>`. That declaration and the `# ..` block above it are its
//!   signature and documentation.
//! - **Described.** Typed by a rule in the checker with no declaration to read
//!   (the operators, and the reductions whose signature is derived from their
//!   step's), so the signature and prose are written here instead. The
//!   signature uses AIPL's declaration syntax so it reads like the others, but
//!   it is a description, not a declaration the compiler checks.
//!
//! The name is not a field: a builtin's canonical name is the one
//! [`Callee::name`] returns, and its import spelling is [`Builtin::name`]
//! (that less the `__builtin_` prefix), so a third copy would only have a way
//! to disagree.
//!
//! [`BUILTINS`] is the whole importable set, in the order the arms of
//! [`Callee`] list them. The names the language reserves without an import and
//! the intrinsics passes synthesize are not here; they are [`Callee`]'s own.

use std::path::PathBuf;

use crate::ast::Callee;
use crate::callee::SeqShape;

/// Where a builtin's signature and documentation are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Definition {
    /// Written in AIPL, in this file of `aipl-mono/src/`.
    Aipl { file: &'static str },
    /// Declared as `__builtin_<name>` in [`crate::BUILTIN_SIGNATURES`].
    Declared,
    /// Neither written nor declared; described here.
    Described {
        /// The signature on one line, as a declaration:
        /// `fn len<T: any>(self: T[]) -> u64`.
        signature: &'static str,
        /// What the builtin does, as the prose a `# ..` block would hold.
        doc: &'static str,
    },
}

/// One importable builtin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Builtin {
    /// The arm of [`Callee`] a call to it carries. A shape-variadic builtin is
    /// listed in its undecided ([`SeqShape::Seq`]) form.
    pub callee: Callee,
    pub definition: Definition,
    /// The other names its AIPL source makes importable: a type it defines and
    /// the cases of a variant among them. Importing any of them loads the
    /// source, as importing the builtin does.
    pub exports: &'static [&'static str],
}

/// The directory the AIPL sources live in, `aipl-mono/src/`: resolved at
/// compile time but *read at run time*. `include_str!` would make every edit to
/// one of these files a source change to this crate, rebuilding it, everything
/// downstream and every test binary — paid on a plain edit, and again on each
/// one the test harness's `fill_expected` refreshes.
const SOURCE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../aipl-mono/src");

impl Builtin {
    const fn aipl(callee: Callee, file: &'static str) -> Builtin {
        Builtin {
            callee,
            definition: Definition::Aipl { file },
            exports: &[],
        }
    }

    /// Written in AIPL like [`Builtin::aipl`], and also exporting `exports` from
    /// its source.
    const fn aipl_exporting(
        callee: Callee,
        file: &'static str,
        exports: &'static [&'static str],
    ) -> Builtin {
        Builtin {
            callee,
            definition: Definition::Aipl { file },
            exports,
        }
    }

    const fn declared(callee: Callee) -> Builtin {
        Builtin {
            callee,
            definition: Definition::Declared,
            exports: &[],
        }
    }

    const fn described(callee: Callee, signature: &'static str, doc: &'static str) -> Builtin {
        Builtin {
            callee,
            definition: Definition::Described { signature, doc },
            exports: &[],
        }
    }

    /// Every importable builtin.
    pub fn all() -> &'static [Builtin] {
        BUILTINS
    }

    /// The builtin `callee` is, or `None` for anything that is not importable
    /// (a user function, `some`, an intrinsic). A shape-variadic builtin is
    /// found whatever shape mono decided for it.
    pub fn of(callee: &Callee) -> Option<&'static Builtin> {
        let undecided = callee.with_seq_shape(SeqShape::Seq);
        BUILTINS.iter().find(|b| b.callee == undecided)
    }

    /// The builtin an import list spells `name` — the bare `len`, not
    /// `__builtin_len` — or `None`.
    pub fn named(name: &str) -> Option<&'static Builtin> {
        BUILTINS.iter().find(|b| b.name() == name)
    }

    /// The builtin whose source exports `name` — a type or a case — or `None`.
    pub fn exporting_name(name: &str) -> Option<&'static Builtin> {
        BUILTINS.iter().find(|b| b.exports.contains(&name))
    }

    /// The builtin whose AIPL source is the file at `path`, judged by its file
    /// name.
    pub fn implemented_at(path: &std::path::Path) -> Option<&'static Builtin> {
        let file = path.file_name()?.to_str()?;
        BUILTINS
            .iter()
            .find(|b| matches!(b.definition, Definition::Aipl { file: f } if f == file))
    }

    /// The spelling an import list uses.
    pub fn name(&self) -> &str {
        self.callee.import_name()
    }

    /// Whether it is written in AIPL.
    pub fn is_aipl_implemented(&self) -> bool {
        matches!(self.definition, Definition::Aipl { .. })
    }

    /// The path of its AIPL source, or `None` for a described builtin.
    pub fn source_path(&self) -> Option<PathBuf> {
        match self.definition {
            Definition::Aipl { file } => Some(PathBuf::from(SOURCE_DIR).join(file)),
            Definition::Declared | Definition::Described { .. } => None,
        }
    }
}

const BUILTINS: &[Builtin] = &[
    Builtin::declared(Callee::Print),
    Builtin::declared(Callee::Split),
    Builtin::declared(Callee::SplitLen),
    Builtin::declared(Callee::Join),
    Builtin::aipl(Callee::Intersperse, "builtin_intersperse.aipl"),
    Builtin::aipl(Callee::TupleWindows, "builtin_tuple_windows.aipl"),
    Builtin::declared(Callee::SameCase),
    Builtin::declared(Callee::CaseName),
    Builtin::declared(Callee::CaseOf),
    Builtin::aipl(Callee::CountIsLessThan, "builtin_count_is_less_than.aipl"),
    Builtin::aipl(Callee::CountIsAtMost, "builtin_count_is_at_most.aipl"),
    Builtin::aipl(
        Callee::CountIsGreaterThan,
        "builtin_count_is_greater_than.aipl",
    ),
    Builtin::aipl(Callee::CountIsAtLeast, "builtin_count_is_at_least.aipl"),
    Builtin::aipl(Callee::CountIsEqual, "builtin_count_is_equal.aipl"),
    Builtin::aipl(Callee::CountIsNotEqual, "builtin_count_is_not_equal.aipl"),
    Builtin::aipl(
        Callee::CountIfIsLessThan,
        "builtin_count_if_is_less_than.aipl",
    ),
    Builtin::aipl(Callee::CountIfIsAtMost, "builtin_count_if_is_at_most.aipl"),
    Builtin::aipl(
        Callee::CountIfIsGreaterThan,
        "builtin_count_if_is_greater_than.aipl",
    ),
    Builtin::aipl(
        Callee::CountIfIsAtLeast,
        "builtin_count_if_is_at_least.aipl",
    ),
    Builtin::aipl(Callee::CountIfIsEqual, "builtin_count_if_is_equal.aipl"),
    Builtin::aipl(
        Callee::CountIfIsNotEqual,
        "builtin_count_if_is_not_equal.aipl",
    ),
    Builtin::aipl(Callee::First, "builtin_first.aipl"),
    Builtin::aipl(Callee::GridGet, "builtin_grid_get.aipl"),
    Builtin::aipl(Callee::GridFrom, "builtin_grid_from.aipl"),
    Builtin::aipl(Callee::GridTrySet, "builtin_grid_try_set.aipl"),
    Builtin::aipl(Callee::Width, "builtin_width.aipl"),
    Builtin::aipl(Callee::SeededWithInt, "builtin_seeded_with_int.aipl"),
    Builtin::aipl(Callee::SeededWithClock, "builtin_seeded_with_clock.aipl"),
    Builtin::aipl(Callee::Height, "builtin_height.aipl"),
    Builtin::aipl(Callee::IsInBoundsOf, "builtin_is_in_bounds_of.aipl"),
    Builtin::aipl(
        Callee::GridIsInBoundsOf,
        "builtin_grid_is_in_bounds_of.aipl",
    ),
    Builtin::aipl(Callee::Last, "builtin_last.aipl"),
    Builtin::aipl(Callee::NonemptyFirst, "builtin_nonempty_first.aipl"),
    Builtin::aipl(Callee::NonemptyLast, "builtin_nonempty_last.aipl"),
    Builtin::aipl(Callee::EnsureNonempty, "builtin_ensure_nonempty.aipl"),
    Builtin::declared(Callee::AssumeNonempty),
    Builtin::aipl(Callee::DropFirst, "builtin_drop_first.aipl"),
    Builtin::aipl(Callee::DropLast, "builtin_drop_last.aipl"),
    Builtin::aipl(Callee::DropN, "builtin_drop_n.aipl"),
    Builtin::aipl(Callee::DropLastN, "builtin_drop_last_n.aipl"),
    Builtin::declared(Callee::ToStr),
    Builtin::declared(Callee::ToStrLen),
    Builtin::declared(Callee::Map),
    Builtin::aipl(Callee::TryMap, "builtin_try_map.aipl"),
    Builtin::declared(Callee::Filter),
    Builtin::declared(Callee::FilterMap),
    Builtin::aipl(Callee::All, "builtin_all.aipl"),
    Builtin::aipl(Callee::Any, "builtin_any.aipl"),
    Builtin::aipl(Callee::LeftFold, "builtin_left_fold.aipl"),
    Builtin::aipl(Callee::RightFold, "builtin_right_fold.aipl"),
    Builtin::aipl(Callee::OptLeftFold, "builtin_opt_left_fold.aipl"),
    Builtin::aipl(Callee::OptRightFold, "builtin_opt_right_fold.aipl"),
    Builtin::declared(Callee::ZipWith),
    Builtin::declared(Callee::Trim),
    Builtin::declared(Callee::HeapStr),
    Builtin::declared(Callee::RopeStr),
    Builtin::aipl(Callee::InlineStr, "builtin_inline_str.aipl"),
    Builtin::declared(Callee::PackInline),
    Builtin::declared(Callee::InlineCapacity),
    Builtin::aipl(Callee::Measure, "builtin_measure.aipl"),
    Builtin::declared(Callee::MeasureBegin),
    Builtin::declared(Callee::MeasureConsume),
    Builtin::declared(Callee::MeasureEnd),
    Builtin::declared(Callee::MeasureRead),
    Builtin::aipl(Callee::IsAllWhitespace, "builtin_is_all_whitespace.aipl"),
    Builtin::declared(Callee::StartsWith(SeqShape::Seq)),
    Builtin::declared(Callee::StartsWithAt(SeqShape::Seq)),
    Builtin::declared(Callee::EndsWith(SeqShape::Seq)),
    Builtin::declared(Callee::Len),
    Builtin::aipl(Callee::Range, "builtin_range.aipl"),
    Builtin::declared(Callee::IsNonempty),
    Builtin::declared(Callee::IsEmpty),
    Builtin::declared(Callee::Push),
    Builtin::declared(Callee::Extend),
    Builtin::aipl(Callee::ExtendOptional, "builtin_extend_optional.aipl"),
    Builtin::declared(Callee::Reserve),
    Builtin::declared(Callee::IsSome),
    Builtin::aipl(Callee::IsSomeAnd, "builtin_is_some_and.aipl"),
    Builtin::aipl(Callee::IsErrAnd, "builtin_is_err_and.aipl"),
    Builtin::aipl(Callee::MapErr, "builtin_map_err.aipl"),
    Builtin::aipl(Callee::MapOk, "builtin_map_ok.aipl"),
    Builtin::aipl(Callee::IntParse, "builtin_int_parse.aipl"),
    Builtin::aipl(Callee::IntParseUnsigned, "builtin_int_parse_unsigned.aipl"),
    Builtin::declared(Callee::IsSpace),
    Builtin::declared(Callee::IsWhitespace),
    Builtin::declared(Callee::IsDigit),
    Builtin::declared(Callee::ToDigit),
    Builtin::declared(Callee::Narrow),
    Builtin::aipl(Callee::TrimWhile, "builtin_trim_while.aipl"),
    Builtin::aipl(Callee::Words, "builtin_words.aipl"),
    Builtin::aipl(Callee::CartesianProduct, "builtin_cartesian_product.aipl"),
    Builtin::aipl(Callee::Count, "builtin_count.aipl"),
    Builtin::aipl(Callee::CountWhile, "builtin_count_while.aipl"),
    Builtin::aipl(Callee::CountIf, "builtin_count_if.aipl"),
    Builtin::aipl(Callee::FindIf, "builtin_find_if.aipl"),
    Builtin::aipl(Callee::MapFindIf, "builtin_map_find_if.aipl"),
    Builtin::aipl(Callee::MapJoin, "builtin_map_join.aipl"),
    Builtin::aipl(Callee::JoinIsNonempty, "builtin_join_is_nonempty.aipl"),
    Builtin::aipl(Callee::JoinIsEmpty, "builtin_join_is_empty.aipl"),
    Builtin::aipl(Callee::Reduce, "builtin_reduce.aipl"),
    Builtin::aipl(Callee::MapReduce, "builtin_map_reduce.aipl"),
    Builtin::aipl(Callee::FilterMapReduce, "builtin_filter_map_reduce.aipl"),
    Builtin::aipl(Callee::SplitMap, "builtin_split_map.aipl"),
    Builtin::aipl(Callee::FindMap, "builtin_find_map.aipl"),
    Builtin::aipl(Callee::ReverseFindMap, "builtin_reverse_find_map.aipl"),
    Builtin::aipl(Callee::FindIndex, "builtin_find_index.aipl"),
    Builtin::aipl(Callee::ValueOr, "builtin_value_or.aipl"),
    Builtin::aipl(Callee::ValueOrErr, "builtin_value_or_err.aipl"),
    Builtin::declared(Callee::Contains(SeqShape::Seq)),
    Builtin::declared(Callee::Has),
    Builtin::declared(Callee::ToArray),
    Builtin::declared(Callee::ToSet),
    Builtin::declared(Callee::ReadFileToString),
    Builtin::declared(Callee::WriteStringToFile),
    Builtin::declared(Callee::ListFiles),
    Builtin::declared(Callee::ReadLine),
    Builtin::declared(Callee::ReadByte),
    Builtin::declared(Callee::ClearTerminal),
    Builtin::declared(Callee::MoveTo),
    Builtin::aipl_exporting(
        Callee::ReadKey,
        "builtin_read_key.aipl",
        &["Key", "Up", "Down", "Left", "Right", "Other"],
    ),
    Builtin::declared(Callee::RandomU64),
    Builtin::declared(Callee::NowNanos),
    Builtin::declared(Callee::MonotonicNow),
    Builtin::declared(Callee::ExecuteProgram),
    Builtin::declared(Callee::Union),
    Builtin::aipl(Callee::UnionAll, "builtin_union_all.aipl"),
    Builtin::aipl(Callee::SetMap, "builtin_set_map.aipl"),
    Builtin::declared(Callee::Get),
    Builtin::declared(Callee::ContainsKey),
    Builtin::declared(Callee::RemoveKey),
    Builtin::declared(Callee::InsertKey),
    Builtin::aipl(Callee::ToDict, "builtin_to_dict.aipl"),
    Builtin::aipl(Callee::MapToDict, "builtin_map_to_dict.aipl"),
    Builtin::declared(Callee::Hash),
    Builtin::declared(Callee::Min),
    Builtin::declared(Callee::Max),
    Builtin::described(
        Callee::Minimum,
        "fn minimum<T: ord>(self: T[]) -> T?",
        "The smallest element, or `none` if the array is empty.",
    ),
    Builtin::described(
        Callee::Maximum,
        "fn maximum<T: ord>(self: T[]) -> T?",
        "The largest element, or `none` if the array is empty.",
    ),
    Builtin::declared(Callee::Reverse),
    Builtin::declared(Callee::Sort),
    Builtin::aipl(Callee::SortBy, "builtin_sort_by.aipl"),
    Builtin::aipl(Callee::Repeat, "builtin_repeat.aipl"),
    Builtin::declared(Callee::With),
    Builtin::declared(Callee::TrySet),
    Builtin::described(
        Callee::WrappingAdd,
        "fn wrapping_add<T: int>(a: T, b: T) -> T",
        "Addition that wraps around on overflow. Import it as the operator: \
         `wrapping_add as +`.",
    ),
    Builtin::described(
        Callee::SaturatingAdd,
        "fn saturating_add<T: int>(a: T, b: T) -> T",
        "Addition that stops at the type's largest value on overflow. Import it as the \
         operator: `saturating_add as +`.",
    ),
    Builtin::described(
        Callee::WrappingSub,
        "fn wrapping_sub<T: int>(a: T, b: T) -> T",
        "Subtraction that wraps around on overflow. Import it as the operator: \
         `wrapping_sub as -`.",
    ),
    Builtin::described(
        Callee::SaturatingSub,
        "fn saturating_sub<T: int>(a: T, b: T) -> T",
        "Subtraction that stops at the type's smallest value on overflow. Import it as the \
         operator: `saturating_sub as -`.",
    ),
    Builtin::described(
        Callee::WrappingMul,
        "fn wrapping_mul<T: int>(a: T, b: T) -> T",
        "Multiplication that wraps around on overflow. Import it as the operator: \
         `wrapping_mul as *`.",
    ),
    Builtin::described(
        Callee::SaturatingDivide,
        "fn saturating_divide<T: int>(a: T, b: T) -> T",
        "Division that never traps: it saturates where the result would not fit. Import it \
         as the operator: `saturating_divide as /`.",
    ),
    Builtin::described(
        Callee::SaturatingRemainder,
        "fn saturating_remainder<T: int>(a: T, b: T) -> T",
        "The remainder of division, never trapping. Import it as the operator: \
         `saturating_remainder as %`.",
    ),
    Builtin::described(
        Callee::Equal,
        "fn equal<T: any>(a: T, b: T) -> bool",
        "Whether the two values are equal. Import it as the operator: `equal as ==`.",
    ),
    Builtin::described(
        Callee::NotEqual,
        "fn not_equal<T: any>(a: T, b: T) -> bool",
        "Whether the two values differ. Import it as the operator: `not_equal as !=`.",
    ),
    Builtin::described(
        Callee::LessThan,
        "fn less_than<T: ord>(a: T, b: T) -> bool",
        "Whether `a` is less than `b`. Import it as the operator: `less_than as <`.",
    ),
    Builtin::described(
        Callee::GreaterThan,
        "fn greater_than<T: ord>(a: T, b: T) -> bool",
        "Whether `a` is greater than `b`. Import it as the operator: `greater_than as >`.",
    ),
    Builtin::described(
        Callee::LessThanOrEqual,
        "fn less_than_or_equal<T: ord>(a: T, b: T) -> bool",
        "Whether `a` is less than or equal to `b`. Import it as the operator: \
         `less_than_or_equal as <=`.",
    ),
    Builtin::described(
        Callee::GreaterThanOrEqual,
        "fn greater_than_or_equal<T: ord>(a: T, b: T) -> bool",
        "Whether `a` is greater than or equal to `b`. Import it as the operator: \
         `greater_than_or_equal as >=`.",
    ),
    Builtin::described(
        Callee::LogicalAnd,
        "fn logical_and(a: bool, b: bool) -> bool",
        "Whether both are true; `b` is not evaluated when `a` is false. Import it as the \
         operator: `logical_and as &&`.",
    ),
    Builtin::described(
        Callee::LogicalOr,
        "fn logical_or(a: bool, b: bool) -> bool",
        "Whether either is true; `b` is not evaluated when `a` is true. Import it as the \
         operator: `logical_or as ||`.",
    ),
    Builtin::described(
        Callee::LogicalNot,
        "fn logical_not(a: bool) -> bool",
        "Whether `a` is false. Import it as the operator: `logical_not as !`.",
    ),
    Builtin::described(
        Callee::Concat,
        "fn concat<T: any>(a: T[], b: T[]) -> T[]",
        "The elements of `a` followed by those of `b`. Also joins two `str`s. Import it as \
         the operator: `concat as +++`.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builtin_is_found_by_callee_and_by_name() {
        let len = Builtin::named("len").expect("`len` is a builtin");
        assert_eq!(len.callee, Callee::Len);
        assert_eq!(Builtin::of(&Callee::Len), Some(len));
        assert_eq!(Builtin::named("__builtin_len"), None);
        assert_eq!(Builtin::of(&Callee::User("len".into())), None);
        assert_eq!(Builtin::of(&Callee::Some), None);
    }

    #[test]
    fn a_shape_variadic_builtin_is_found_whatever_shape_was_decided() {
        let decided = Callee::StartsWith(SeqShape::Elem);
        assert_eq!(Builtin::of(&decided).map(|b| b.name()), Some("starts_with"));
    }

    #[test]
    fn each_builtin_appears_once_under_its_own_canonical_name() {
        for (i, b) in BUILTINS.iter().enumerate() {
            assert_eq!(
                Callee::from_canonical(b.callee.name()).as_ref(),
                Some(&b.callee),
                "{}",
                b.callee
            );
            assert!(
                BUILTINS[..i]
                    .iter()
                    .all(|earlier| earlier.callee != b.callee),
                "`{}` is listed twice",
                b.name()
            );
        }
    }

    #[test]
    fn a_description_declares_the_function_it_describes() {
        for b in BUILTINS {
            if let Definition::Described { signature, doc } = b.definition {
                assert!(
                    signature.starts_with(&format!("fn {}", b.name())),
                    "`{}`: signature `{signature}`",
                    b.name()
                );
                assert!(!doc.is_empty(), "`{}` has no documentation", b.name());
            }
        }
    }

    /// The kind a builtin is filed under has to be the kind it is: a "declared"
    /// one with no declaration would show nothing, and a "described" one with a
    /// declaration would say something the declaration does not.
    #[test]
    fn declared_builtins_are_exactly_those_with_a_declaration() {
        for b in BUILTINS {
            let declaration = format!("\nfn __builtin_{}", b.name());
            let has_one =
                crate::BUILTIN_SIGNATURES.contains(&declaration) && !b.is_aipl_implemented();
            assert_eq!(
                has_one,
                b.definition == Definition::Declared,
                "`{}` is filed as {:?}",
                b.name(),
                b.definition
            );
        }
    }

    #[test]
    fn an_aipl_source_is_named_for_its_builtin_and_exists() {
        for b in BUILTINS {
            let Some(path) = b.source_path() else {
                continue;
            };
            assert!(path.is_file(), "`{}`: {path:?} is missing", b.name());
            assert_eq!(
                Builtin::implemented_at(&path).map(|found| &found.callee),
                Some(&b.callee)
            );
        }
    }
}
