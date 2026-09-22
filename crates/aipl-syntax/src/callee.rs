//! What an [`ExprKind::Call`](crate::ast::ExprKind::Call) calls.
//!
//! Every function the compiler itself knows — an importable builtin, an
//! operator's canonical impl, a `some`/`ok`/`err` constructor, or one of the
//! intrinsics a pass synthesizes — is its own arm of
//! [`Callee`]; only a name that originates in source (a user function, one of
//! its monomorphized instances, a variant constructor, a local holding a
//! function value) is carried as text, in [`Callee::User`]. So a pass that
//! wants to know "is this `len`?" matches `Callee::Len`, and one that adds a
//! builtin arm without teaching every dispatch about it hears from the
//! exhaustiveness checker rather than from a string that silently never
//! matched.
//!
//! The canonical *name* of an arm — `__builtin_len`, `__assert` — still exists,
//! because a builtin is registered in the function tables, emitted as a symbol
//! and shown in a diagnostic by name; [`Callee::name`] is the one place those
//! spellings live, and [`Callee::from_canonical`] reads them back for the
//! loader, which resolves an imported `len` to the reserved canonical string
//! and then to the arm.

/// How mono resolved the pattern argument of a shape-variadic sequence builtin
/// (`starts_with`/`starts_with_at`/`ends_with`/`contains`): its `T*` parameter
/// accepts a sequence, a single element, or an optional element, and each
/// shape is a distinct lowering in codegen. `Seq` is the shape before mono has
/// decided — the one [`Callee::from_canonical`] yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SeqShape {
    /// The pattern is a sequence of the receiver's element type.
    #[default]
    Seq,
    /// The pattern is one element.
    Elem,
    /// The pattern is an optional element (`none` matches nothing).
    Opt,
}

/// The callee of a call expression. See the [module docs](self).
///
/// The importable builtins come first, in [`Callee::IMPORTABLE`] order, then
/// the operator impls (reached through `OPERATOR_BUILTINS`), then the names the
/// language reserves without an import, then the intrinsics passes synthesize.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Callee {
    // ---- importable builtins: `import { name } from builtins;` ----
    Print,
    Split,
    SplitLen,
    Join,
    Intersperse,
    TupleWindows,
    SameCase,
    CaseName,
    CaseOf,
    CountIsLessThan,
    CountIsAtMost,
    CountIsGreaterThan,
    CountIsAtLeast,
    CountIsEqual,
    CountIsNotEqual,
    First,
    Last,
    DropFirst,
    DropLast,
    DropN,
    DropLastN,
    ToStr,
    Map,
    TryMap,
    Filter,
    FilterMap,
    All,
    Any,
    LeftFold,
    RightFold,
    OptLeftFold,
    OptRightFold,
    ZipWith,
    Trim,
    IsAllWhitespace,
    /// `starts_with(self: T[], prefix: T*)`, with the pattern shape mono chose.
    StartsWith(SeqShape),
    /// `starts_with_at(self: T[], prefix: T*, at: u64)`, likewise.
    StartsWithAt(SeqShape),
    /// `ends_with(self: T[], suffix: T*)`, likewise.
    EndsWith(SeqShape),
    /// `contains(self: T[], needle: T*)`, likewise.
    Contains(SeqShape),
    Len,
    IsNonempty,
    IsEmpty,
    Push,
    Extend,
    Reserve,
    IsSome,
    IsSomeAnd,
    IsErrAnd,
    MapErr,
    MapOk,
    IntParse,
    IsSpace,
    IsWhitespace,
    IsDigit,
    ToDigit,
    TrimWhile,
    Count,
    CountWhile,
    CountIf,
    FindIf,
    MapFindIf,
    MapJoin,
    FindMap,
    ReverseFindMap,
    FindIndex,
    ValueOr,
    ValueOrErr,
    Has,
    ToArray,
    ToSet,
    ReadFileToString,
    WriteStringToFile,
    ListFiles,
    NowNanos,
    MonotonicNow,
    ExecuteProgram,
    Union,
    UnionAll,
    Get,
    ContainsKey,
    Hash,
    Min,
    Max,
    Minimum,
    Maximum,
    Reverse,
    Sort,
    SortBy,
    Repeat,

    // ---- operator impls: `import { wrapping_add as + } from builtins;` ----
    // Intrinsified by codegen; an operator use and a bare `concat(a, b)` are
    // the same call to the same arm.
    WrappingAdd,
    SaturatingAdd,
    WrappingSub,
    SaturatingSub,
    WrappingMul,
    SaturatingDivide,
    SaturatingRemainder,
    Equal,
    NotEqual,
    LessThan,
    GreaterThan,
    LessThanOrEqual,
    GreaterThanOrEqual,
    LogicalAnd,
    LogicalOr,
    LogicalNot,
    Concat,

    // ---- reserved without an import ----
    /// `some(x)`.
    Some,
    /// `ok(x)` / `ok()`.
    Ok,
    /// `err(e)`.
    Err,

    // ---- intrinsics synthesized by the compiler ----
    /// `assert(cond)` inside a `.test({ .. })` body, with its source location
    /// (the parser's `post_parse` rewrite).
    Assert,
    /// The `__test_main` driver's per-test bracketing and final summary.
    TestBegin,
    TestEnd,
    TestSummary,
    /// One interpolation of a template literal: `to_str` under another name,
    /// except that a `char` widens to a one-char `str` (the parser).
    TemplateInterp,
    /// Joins two pieces of a template literal (the parser).
    TemplateConcat,
    /// Array-literal spread (`[..xs, y]`): size the accumulator up front, then
    /// write each piece into that capacity in place (the loader).
    ArrReserve,
    ArrAppend,
    ArrConcat,
    /// An empty array reserved to a given capacity — `map`'s output (mono).
    WithCapacity,
    /// The moved-in array parameter as a writable block, for the in-place
    /// `map`/`filter` lowering (mono).
    ArrWritable,
    /// `__move(x)`: the local binding `x` at its last use, as a value the
    /// caller may hand off rather than borrow. Mono wraps a heap binding's
    /// last-use call argument in it (`move_last_use`); codegen turns the
    /// binding's own reference into a fresh temporary the call then moves in,
    /// so the retain/release pair a borrow would cost is never emitted. On a
    /// binding that holds no reference of its own it is a plain read.
    Move,
    /// In-place `map`: overwrite one slot of the reused buffer (mono).
    MapSet,
    /// In-place `map`: reinterpret the reused buffer as the result type (mono).
    MapResult,
    /// In-place `filter`: keep, drop, or truncate after the kept prefix (mono).
    FilterKeep,
    FilterDrop,
    FilterTruncate,
    /// A single `char` to the one-char `str` holding it, emitted by variadic
    /// `char*` specialization (mono).
    CharToStr,
    /// `for (let v : xs.reverse())` after fusion: `xs` itself, walked backwards
    /// by codegen (mono).
    ReverseIter,

    // ---- names from source ----
    /// A name that is not the compiler's to know: a user function (mangled
    /// `__m<N>__name` after loading, `name$..` once monomorphized), a variant
    /// constructor (`Case@Variant`), a local holding a function value — or,
    /// before the loader has resolved imports, any spelling at all.
    User(String),
}

impl Callee {
    /// Every importable builtin, in registry order: the arms a program brings
    /// into scope with `import { .. } from builtins;` — the by-name callable
    /// builtins. `some`/`ok`/`err`, the conversions and the operators are
    /// language syntax, not importable idents. The shape-variadic arms are
    /// listed in their undecided (`Seq`) form.
    pub const IMPORTABLE: &'static [Callee] = &[
        Callee::Print,
        Callee::Split,
        Callee::SplitLen,
        Callee::Join,
        Callee::Intersperse,
        Callee::TupleWindows,
        Callee::SameCase,
        Callee::CaseName,
        Callee::CaseOf,
        Callee::CountIsLessThan,
        Callee::CountIsAtMost,
        Callee::CountIsGreaterThan,
        Callee::CountIsAtLeast,
        Callee::CountIsEqual,
        Callee::CountIsNotEqual,
        Callee::First,
        Callee::Last,
        Callee::DropFirst,
        Callee::DropLast,
        Callee::DropN,
        Callee::DropLastN,
        Callee::ToStr,
        Callee::Map,
        Callee::TryMap,
        Callee::Filter,
        Callee::FilterMap,
        Callee::All,
        Callee::Any,
        Callee::LeftFold,
        Callee::RightFold,
        Callee::OptLeftFold,
        Callee::OptRightFold,
        Callee::ZipWith,
        Callee::Trim,
        Callee::IsAllWhitespace,
        Callee::StartsWith(SeqShape::Seq),
        Callee::StartsWithAt(SeqShape::Seq),
        Callee::EndsWith(SeqShape::Seq),
        Callee::Len,
        Callee::IsNonempty,
        Callee::IsEmpty,
        Callee::Push,
        Callee::Extend,
        Callee::Reserve,
        Callee::IsSome,
        Callee::IsSomeAnd,
        Callee::IsErrAnd,
        Callee::MapErr,
        Callee::MapOk,
        Callee::IntParse,
        Callee::IsSpace,
        Callee::IsWhitespace,
        Callee::IsDigit,
        Callee::ToDigit,
        Callee::TrimWhile,
        Callee::Count,
        Callee::CountWhile,
        Callee::CountIf,
        Callee::FindIf,
        Callee::MapFindIf,
        Callee::MapJoin,
        Callee::FindMap,
        Callee::ReverseFindMap,
        Callee::FindIndex,
        Callee::ValueOr,
        Callee::ValueOrErr,
        Callee::Contains(SeqShape::Seq),
        Callee::Has,
        Callee::ToArray,
        Callee::ToSet,
        Callee::ReadFileToString,
        Callee::WriteStringToFile,
        Callee::ListFiles,
        Callee::NowNanos,
        Callee::MonotonicNow,
        Callee::ExecuteProgram,
        Callee::Union,
        Callee::UnionAll,
        Callee::Get,
        Callee::ContainsKey,
        Callee::Hash,
        Callee::Min,
        Callee::Max,
        Callee::Minimum,
        Callee::Maximum,
        Callee::Reverse,
        Callee::Sort,
        Callee::SortBy,
        Callee::Repeat,
    ];

    /// The operator impls: every arm an operator spelling can resolve to.
    pub const OPERATORS: &'static [Callee] = &[
        Callee::WrappingAdd,
        Callee::SaturatingAdd,
        Callee::WrappingSub,
        Callee::SaturatingSub,
        Callee::WrappingMul,
        Callee::SaturatingDivide,
        Callee::SaturatingRemainder,
        Callee::Equal,
        Callee::NotEqual,
        Callee::LessThan,
        Callee::GreaterThan,
        Callee::LessThanOrEqual,
        Callee::GreaterThanOrEqual,
        Callee::LogicalAnd,
        Callee::LogicalOr,
        Callee::LogicalNot,
        Callee::Concat,
    ];

    /// The reserved and synthesized arms — everything
    /// [`Callee::from_canonical`] can yield beyond the two lists above.
    const INTERNAL: &'static [Callee] = &[
        Callee::Some,
        Callee::Ok,
        Callee::Err,
        Callee::Assert,
        Callee::TestBegin,
        Callee::TestEnd,
        Callee::TestSummary,
        Callee::TemplateInterp,
        Callee::TemplateConcat,
        Callee::ArrReserve,
        Callee::ArrAppend,
        Callee::ArrConcat,
        Callee::WithCapacity,
        Callee::ArrWritable,
        Callee::Move,
        Callee::MapSet,
        Callee::MapResult,
        Callee::FilterKeep,
        Callee::FilterDrop,
        Callee::FilterTruncate,
        Callee::CharToStr,
        Callee::ReverseIter,
    ];

    /// The canonical name: the reserved `__builtin_*` spelling of a builtin
    /// (which no user identifier can be, so a user item can never shadow one),
    /// the `__`-prefixed name of an intrinsic, the bare `some`/`ok`/`err`/
    /// `i64`, or a [`Callee::User`]'s text. This is the name the function
    /// tables are keyed by and the symbol a builtin is emitted or registered
    /// under; it does not carry a shape-variadic arm's shape, which is data on
    /// the arm rather than a suffix on the string.
    pub fn name(&self) -> &str {
        match self {
            Callee::Print => "__builtin_print",
            Callee::Split => "__builtin_split",
            Callee::SplitLen => "__builtin_split_len",
            Callee::Join => "__builtin_join",
            Callee::Intersperse => "__builtin_intersperse",
            Callee::TupleWindows => "__builtin_tuple_windows",
            Callee::SameCase => "__builtin_same_case",
            Callee::CaseName => "__builtin_case_name",
            Callee::CaseOf => "__builtin_case_of",
            Callee::CountIsLessThan => "__builtin_count_is_less_than",
            Callee::CountIsAtMost => "__builtin_count_is_at_most",
            Callee::CountIsGreaterThan => "__builtin_count_is_greater_than",
            Callee::CountIsAtLeast => "__builtin_count_is_at_least",
            Callee::CountIsEqual => "__builtin_count_is_equal",
            Callee::CountIsNotEqual => "__builtin_count_is_not_equal",
            Callee::First => "__builtin_first",
            Callee::Last => "__builtin_last",
            Callee::DropFirst => "__builtin_drop_first",
            Callee::DropLast => "__builtin_drop_last",
            Callee::DropN => "__builtin_drop_n",
            Callee::DropLastN => "__builtin_drop_last_n",
            Callee::ToStr => "__builtin_to_str",
            Callee::Map => "__builtin_map",
            Callee::TryMap => "__builtin_try_map",
            Callee::Filter => "__builtin_filter",
            Callee::FilterMap => "__builtin_filter_map",
            Callee::All => "__builtin_all",
            Callee::Any => "__builtin_any",
            Callee::LeftFold => "__builtin_left_fold",
            Callee::RightFold => "__builtin_right_fold",
            Callee::OptLeftFold => "__builtin_opt_left_fold",
            Callee::OptRightFold => "__builtin_opt_right_fold",
            Callee::ZipWith => "__builtin_zip_with",
            Callee::Trim => "__builtin_trim",
            Callee::IsAllWhitespace => "__builtin_is_all_whitespace",
            Callee::StartsWith(_) => "__builtin_starts_with",
            Callee::StartsWithAt(_) => "__builtin_starts_with_at",
            Callee::EndsWith(_) => "__builtin_ends_with",
            Callee::Contains(_) => "__builtin_contains",
            Callee::Len => "__builtin_len",
            Callee::IsNonempty => "__builtin_is_nonempty",
            Callee::IsEmpty => "__builtin_is_empty",
            Callee::Push => "__builtin_push",
            Callee::Extend => "__builtin_extend",
            Callee::Reserve => "__builtin_reserve",
            Callee::IsSome => "__builtin_is_some",
            Callee::IsSomeAnd => "__builtin_is_some_and",
            Callee::IsErrAnd => "__builtin_is_err_and",
            Callee::MapErr => "__builtin_map_err",
            Callee::MapOk => "__builtin_map_ok",
            Callee::IntParse => "__builtin_int_parse",
            Callee::IsSpace => "__builtin_is_space",
            Callee::IsWhitespace => "__builtin_is_whitespace",
            Callee::IsDigit => "__builtin_is_digit",
            Callee::ToDigit => "__builtin_to_digit",
            Callee::TrimWhile => "__builtin_trim_while",
            Callee::Count => "__builtin_count",
            Callee::CountWhile => "__builtin_count_while",
            Callee::CountIf => "__builtin_count_if",
            Callee::FindIf => "__builtin_find_if",
            Callee::MapFindIf => "__builtin_map_find_if",
            Callee::MapJoin => "__builtin_map_join",
            Callee::FindMap => "__builtin_find_map",
            Callee::ReverseFindMap => "__builtin_reverse_find_map",
            Callee::FindIndex => "__builtin_find_index",
            Callee::ValueOr => "__builtin_value_or",
            Callee::ValueOrErr => "__builtin_value_or_err",
            Callee::Has => "__builtin_has",
            Callee::ToArray => "__builtin_to_array",
            Callee::ToSet => "__builtin_to_set",
            Callee::ReadFileToString => "__builtin_read_file_to_string",
            Callee::WriteStringToFile => "__builtin_write_string_to_file",
            Callee::ListFiles => "__builtin_list_files",
            Callee::NowNanos => "__builtin_now_nanos",
            Callee::MonotonicNow => "__builtin_monotonic_now",
            Callee::ExecuteProgram => "__builtin_execute_program",
            Callee::Union => "__builtin_union",
            Callee::UnionAll => "__builtin_union_all",
            Callee::Get => "__builtin_get",
            Callee::ContainsKey => "__builtin_contains_key",
            Callee::Hash => "__builtin_hash",
            Callee::Min => "__builtin_min",
            Callee::Max => "__builtin_max",
            Callee::Minimum => "__builtin_minimum",
            Callee::Maximum => "__builtin_maximum",
            Callee::Reverse => "__builtin_reverse",
            Callee::Sort => "__builtin_sort",
            Callee::SortBy => "__builtin_sort_by",
            Callee::Repeat => "__builtin_repeat",

            Callee::WrappingAdd => "__builtin_wrapping_add",
            Callee::SaturatingAdd => "__builtin_saturating_add",
            Callee::WrappingSub => "__builtin_wrapping_sub",
            Callee::SaturatingSub => "__builtin_saturating_sub",
            Callee::WrappingMul => "__builtin_wrapping_mul",
            Callee::SaturatingDivide => "__builtin_saturating_divide",
            Callee::SaturatingRemainder => "__builtin_saturating_remainder",
            Callee::Equal => "__builtin_equal",
            Callee::NotEqual => "__builtin_not_equal",
            Callee::LessThan => "__builtin_less_than",
            Callee::GreaterThan => "__builtin_greater_than",
            Callee::LessThanOrEqual => "__builtin_less_than_or_equal",
            Callee::GreaterThanOrEqual => "__builtin_greater_than_or_equal",
            Callee::LogicalAnd => "__builtin_logical_and",
            Callee::LogicalOr => "__builtin_logical_or",
            Callee::LogicalNot => "__builtin_logical_not",
            Callee::Concat => "__builtin_concat",

            Callee::Some => "some",
            Callee::Ok => "ok",
            Callee::Err => "err",

            Callee::Assert => "__assert",
            Callee::TestBegin => "__test_begin",
            Callee::TestEnd => "__test_end",
            Callee::TestSummary => "__test_summary",
            Callee::TemplateInterp => "__template_interp",
            Callee::TemplateConcat => "__aipl_concat",
            Callee::ArrReserve => "__aipl_arr_reserve",
            Callee::ArrAppend => "__aipl_arr_append",
            Callee::ArrConcat => "__aipl_arr_concat",
            Callee::WithCapacity => "__builtin_with_capacity",
            Callee::ArrWritable => "__arr_writable",
            Callee::Move => "__move",
            Callee::MapSet => "__map_set",
            Callee::MapResult => "__map_result",
            Callee::FilterKeep => "__filter_keep",
            Callee::FilterDrop => "__filter_drop",
            Callee::FilterTruncate => "__filter_truncate",
            Callee::CharToStr => "__char_to_str",
            Callee::ReverseIter => "__reverse_iter",

            Callee::User(s) => s,
        }
    }

    /// The name as a diagnostic shows it: a builtin's importable spelling
    /// (`len`, not `__builtin_len` — a reserved prefix the reader could not
    /// find in their own source), anything else as is.
    pub fn display_name(&self) -> &str {
        self.import_name()
    }

    /// The arm whose canonical [`name`](Callee::name) is `name`, or `None` for
    /// a name the compiler does not know — the loader's resolution step, after
    /// an import has mapped a spelling to its reserved canonical string. A
    /// shape-variadic builtin comes back undecided ([`SeqShape::Seq`]).
    pub fn from_canonical(name: &str) -> Option<Callee> {
        Callee::IMPORTABLE
            .iter()
            .chain(Callee::OPERATORS)
            .chain(Callee::INTERNAL)
            .find(|c| c.name() == name)
            .cloned()
    }

    /// `name` as a callee: the arm it canonically names, else
    /// [`Callee::User`] holding it.
    pub fn resolve(name: String) -> Callee {
        Callee::from_canonical(&name).unwrap_or(Callee::User(name))
    }

    /// The importable builtin spelled `name` — the bare `len`, as an import
    /// list writes it — or `None`. What `import { len } from builtins;`
    /// resolves through.
    pub fn importable(name: &str) -> Option<Callee> {
        Callee::IMPORTABLE
            .iter()
            .find(|c| c.import_name() == name)
            .cloned()
    }

    /// The spelling an import list uses for an importable builtin: its
    /// canonical name less the `__builtin_` prefix. For any other arm, the
    /// canonical name unchanged.
    pub fn import_name(&self) -> &str {
        self.name()
            .strip_prefix("__builtin_")
            .unwrap_or(self.name())
    }

    /// Whether this callee has a canonical `__builtin_*` name — an importable
    /// builtin or an operator impl (or the reserved-prefix `with_capacity`
    /// intrinsic): the set the loader's function tables register under that
    /// prefix and that no source identifier can name.
    pub fn is_builtin(&self) -> bool {
        !matches!(self, Callee::User(_)) && self.name().starts_with("__builtin_")
    }

    /// The text of a [`Callee::User`], or `None` for every arm the compiler
    /// knows.
    pub fn user(&self) -> Option<&str> {
        match self {
            Callee::User(s) => Some(s),
            _ => None,
        }
    }

    /// The pattern shape of a shape-variadic sequence builtin, or `None` for
    /// any other arm.
    pub fn seq_shape(&self) -> Option<SeqShape> {
        match self {
            Callee::StartsWith(s)
            | Callee::StartsWithAt(s)
            | Callee::EndsWith(s)
            | Callee::Contains(s) => Some(*s),
            _ => None,
        }
    }

    /// This callee with its pattern shape set to `shape`. Only the four
    /// shape-variadic arms carry one; every other arm is returned unchanged.
    pub fn with_seq_shape(&self, shape: SeqShape) -> Callee {
        match self {
            Callee::StartsWith(_) => Callee::StartsWith(shape),
            Callee::StartsWithAt(_) => Callee::StartsWithAt(shape),
            Callee::EndsWith(_) => Callee::EndsWith(shape),
            Callee::Contains(_) => Callee::Contains(shape),
            other => other.clone(),
        }
    }
}

impl PartialEq<str> for Callee {
    /// A callee equals a string when that string is its canonical name — so
    /// `callee == "__m3__foo"` reads a [`Callee::User`] naturally. Comparing
    /// against a builtin's canonical spelling works too, but a pattern on the
    /// arm is the intended form.
    fn eq(&self, other: &str) -> bool {
        self.name() == other
    }
}

impl PartialEq<&str> for Callee {
    fn eq(&self, other: &&str) -> bool {
        self.name() == *other
    }
}

impl PartialEq<String> for Callee {
    fn eq(&self, other: &String) -> bool {
        self.name() == other
    }
}

impl PartialEq<Callee> for str {
    fn eq(&self, other: &Callee) -> bool {
        self == other.name()
    }
}

impl PartialEq<Callee> for String {
    fn eq(&self, other: &Callee) -> bool {
        self.as_str() == other.name()
    }
}

impl std::fmt::Display for Callee {
    /// The canonical name — what a symbol, a table key or a mangled instance
    /// name spells.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_arm_round_trips_through_its_canonical_name() {
        for c in Callee::IMPORTABLE
            .iter()
            .chain(Callee::OPERATORS)
            .chain(Callee::INTERNAL)
        {
            assert_eq!(Callee::from_canonical(c.name()).as_ref(), Some(c), "{c}");
        }
        assert_eq!(Callee::from_canonical("i64"), None);
        assert_eq!(
            Callee::resolve("__m3__foo".into()),
            Callee::User("__m3__foo".into())
        );
    }

    #[test]
    fn canonical_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in Callee::IMPORTABLE
            .iter()
            .chain(Callee::OPERATORS)
            .chain(Callee::INTERNAL)
        {
            assert!(
                seen.insert(c.name()),
                "duplicate canonical name {}",
                c.name()
            );
        }
    }

    #[test]
    fn importable_names_strip_the_prefix() {
        assert_eq!(Callee::importable("len"), Some(Callee::Len));
        assert_eq!(
            Callee::importable("starts_with"),
            Some(Callee::StartsWith(SeqShape::Seq))
        );
        assert_eq!(Callee::importable("__builtin_len"), None);
        assert_eq!(Callee::importable("wrapping_add"), None);
        assert_eq!(Callee::Len.display_name(), "len");
    }

    #[test]
    fn shape_is_data_not_a_suffix() {
        let c = Callee::StartsWith(SeqShape::Seq).with_seq_shape(SeqShape::Elem);
        assert_eq!(c, Callee::StartsWith(SeqShape::Elem));
        assert_eq!(c.name(), "__builtin_starts_with");
        assert_eq!(Callee::Len.with_seq_shape(SeqShape::Opt), Callee::Len);
    }
}
