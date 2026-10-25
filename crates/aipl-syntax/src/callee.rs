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

use crate::builtin::Builtin;

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
/// The importable builtins come first, in [`crate::builtin::BUILTINS`] order — the
/// operator impls among them, since an operator is a function and its impl is
/// imported like any other — then the names the language reserves without an
/// import, then the intrinsics passes synthesize.
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
    CountIfIsLessThan,
    CountIfIsAtMost,
    CountIfIsGreaterThan,
    CountIfIsAtLeast,
    CountIfIsEqual,
    CountIfIsNotEqual,
    First,
    /// `grid_get(self: T[,], row: u64, column: u64) -> T?` — the cell at `row`,
    /// `column`, or `none` off the grid. What `grid[row, column]` is written as.
    GridGet,
    /// `grid_from(width: u64, cells: T[]) -> T[,]` — a grid of `cells` laid out
    /// `width` to a row. What a nested array literal in a grid position is
    /// rewritten to.
    GridFrom,
    /// `grid_try_set(self: T[,], row: u64, column: u64, value: T) -> T[,]` — the grid
    /// with that cell replaced, or unchanged off the grid. What an indexed write
    /// `set grid[row, column] = value else { .. }` stores.
    GridTrySet,
    /// `width(self: T[,]) -> u64` — how many cells a row of the grid holds.
    Width,
    /// `seeded_with_int(seed) -> RandomProvider`: the provider that starts the
    /// generator at `seed`, for a `shim random { .. }` block.
    SeededWithInt,
    /// `seeded_with_clock() -> RandomProvider`: the provider that seeds the
    /// generator from the clock, for a `shim random { .. }` block.
    SeededWithClock,
    /// `height(self: T[,]) -> u64` — how many rows the grid has.
    Height,
    /// `is_in_bounds_of(self: u64, other: T[]) -> bool` — whether `other[self]` is an
    /// element.
    IsInBoundsOf,
    /// `grid_is_in_bounds_of(self: T[,], row: u64, column: u64) -> bool` — whether the
    /// cell is on the grid. What `(row, column).is_in_bounds_of(grid)` is rewritten to.
    GridIsInBoundsOf,
    Last,
    NonemptyFirst,
    NonemptyLast,
    EnsureNonempty,
    /// `assume_nonempty(self: T[]) -> T[] without []` — the refinement granted
    /// without a test. Internal: only `ensure_nonempty`'s source may call it,
    /// behind the `is_nonempty` check that makes it sound. Erased with the
    /// refinement it introduces (`erase_refinements`), so no pass past the
    /// checker ever sees it.
    AssumeNonempty,
    DropFirst,
    DropLast,
    DropN,
    DropLastN,
    ToStr,
    /// `to_str_len(self: T) -> u64` — the length `to_str(self)` would have,
    /// without building it. What the operation-fusion pass writes for
    /// `v.to_str().len()`; also callable directly.
    ToStrLen,
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
    /// The `*_str` family: the content of the argument in one **named** runtime
    /// representation (`crates/aipl-codegen/src/str24.rs`'s `buffer` / `inline` /
    /// `rope`), rather than whichever one it happens to arrive in.
    ///
    /// `PackInline` and `InlineCapacity` are the unchecked halves `inline_str`
    /// is written from, and are [`crate::INTERNAL_BUILTINS`] — only
    /// `builtin_inline_str.aipl` may reach them.
    HeapStr,
    RopeStr,
    /// `inline_str(self: str) -> str?` — the one of the family that can fail, so
    /// the only one written in AIPL (`builtin_inline_str.aipl`): content past the
    /// inline capacity has no inline form, and the optional says so.
    InlineStr,
    PackInline,
    InlineCapacity,
    /// `measure(label, input, f) -> Measurement` — runs `f(input)` once inside a
    /// counting window and reports what it cost. Written in AIPL
    /// (`builtin_measure.aipl`) on top of the three runtime hooks below, which only
    /// it may reach. Legal only in a `.performance` block: it carries the internal
    /// `!measure` effect, which nothing else declares.
    Measure,
    /// Open the counting window: snapshot the allocation, byte and instruction
    /// counters and start tracking the peak of live bytes from where they stand.
    MeasureBegin,
    /// Release a value inside the window (see `__builtin_measure_consume`).
    MeasureConsume,
    /// Close the window, keeping what happened inside it for [`Callee::MeasureRead`].
    MeasureEnd,
    /// One counter of the closed window, chosen by index.
    MeasureRead,
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
    /// `range(self: T[]) -> Span` — `0..len`, the index range the receiver's
    /// elements sit in. AIPL-implemented (`builtin_range.aipl`).
    Range,
    IsNonempty,
    IsEmpty,
    Push,
    Extend,
    /// `extend_optional(mut self: T[], other: T?)` — the `T?` shape of
    /// `extend`'s variadic source, for an array receiver, which mono resolves
    /// `set xs.extend(maybe)` onto. Internal: `extend` is the spelling, and a
    /// second public name for one of its shapes is the hand-rolled helper it
    /// replaced. A `push` rather than an `extend` of a one-element sequence,
    /// because `extend` reserves exactly what it is given and so reallocates on
    /// every one-element append.
    ExtendOptional,
    Reserve,
    IsSome,
    IsSomeAnd,
    IsErrAnd,
    MapErr,
    MapOk,
    IntParse,
    IntParseUnsigned,
    IsSpace,
    IsWhitespace,
    IsDigit,
    ToDigit,
    Narrow,
    TrimWhile,
    Words,
    CartesianProduct,
    ClearScreen,
    Count,
    CountWhile,
    CountIf,
    FindIf,
    MapFindIf,
    MapJoin,
    JoinIsNonempty,
    JoinIsEmpty,
    Reduce,
    MapReduce,
    FilterMapReduce,
    SplitMap,
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
    ReadLine,
    /// `read_byte() -> u8?` — one byte of standard input, read without waiting for
    /// a line (`!read_stdin_raw`).
    ReadByte,
    /// `read_key() -> Key?` — one key press, an arrow key decoded from the bytes
    /// `read_byte` returns (`!read_stdin_raw`).
    ReadKey,
    /// `random_u64() -> u64` — the next random number. Has no implementation of its
    /// own: it exists only under a `shim random`, whose provider supplies the
    /// generator (see `SHIM_ONLY_EFFECTS`).
    RandomU64,
    NowNanos,
    MonotonicNow,
    ExecuteProgram,
    Union,
    UnionAll,
    /// `set_map(self: #{T}, f: (T) -> U) -> U[]` — `map` for a set, yielding an
    /// array so the result's length is always the receiver's. `Map` has no set
    /// form: a set result would shorten whenever `f` collided two elements.
    SetMap,
    Get,
    ContainsKey,
    RemoveKey,
    /// `insert_key(mut self: #{K: V}, key: K, value: V)` — bind `key` to `value`,
    /// replacing whatever it was bound to. The mutating half `to_dict` and
    /// `map_to_dict` are written with.
    InsertKey,
    ToDict,
    MapToDict,
    Hash,
    Min,
    Max,
    Minimum,
    Maximum,
    Reverse,
    Sort,
    SortBy,
    Repeat,
    With,
    TrySet,

    // ---- operator impls: `import { wrapping_add as + } from builtins;` ----
    // Importable like the rest, by name (`import { concat }` then `concat(a, b)`)
    // or aliased to the operator spelling. Intrinsified by codegen; an operator
    // use and a bare `concat(a, b)` are the same call to the same arm.
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
    /// (the loader's `bake` rewrite).
    Assert,
    /// `trace(expr)` anywhere in a program: the message to print, already
    /// composed from the expression's source location, its own text and its
    /// rendered value (the loader's `bake_traces`).
    ///
    /// Prints, but deliberately carries no `!prints` effect, so a trace can be
    /// dropped into any function without restating its signature. That is what
    /// `aipl check` refusing a program containing one buys back: the escape
    /// hatch stays a debugging one.
    Trace,
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
    /// `for (let part : s.split(sep))` after fusion: the source and the
    /// separator, walked by a codegen-stacked split cursor that hands back one
    /// part per iteration, so the array of parts is never built (mono).
    SplitIter,
    /// `__dict_key_at(d, i)` / `__dict_value_at(d, i)` — the key and the value
    /// of dict `d`'s `i`th pair, by position in its pair array. What mono lowers
    /// a `for (let k, v : d)` into (see its `For` arm): a dict is indexed by its
    /// key, so the loop is the counted walk over the pairs that produces both.
    ///
    /// Positional, so neither is reachable from source — a dict's order is not
    /// promised, and a position is meaningless to write code against. The loop
    /// that synthesizes them is the only caller, and it keeps `i` to itself.
    DictKeyAt,
    DictValueAt,
    /// `__random_next()`: the generator a `shim random <provider>` installs as
    /// `random_u64` — a runtime entry point, never called from source (the parser).
    RandomNext,
    /// `__with_or_self(xs, i, v) -> T[]`: `xs` with element `i` replaced, or `xs`
    /// itself when `i` is past the end — what `try_set` lowers to (`lower_try_set`),
    /// and what `xs.with(i, v).value_or(xs)` is fused into (`fuse_try_with`). One
    /// read of `xs`, so it writes in place when `with` would.
    WithOrSelf,
    /// `__random_seed(seed)`: start the sequence at `seed` (the parser, as the first
    /// thing in a `shim random seeded_with_int(seed)` body).
    RandomSeed,
    /// `__random_seed_from_clock()`: the same, seeded from the clock — an ordinary
    /// `now_nanos` reading, so it carries `!clock` (the parser, for
    /// `shim random seeded_with_clock()`).
    RandomSeedFromClock,

    // ---- names from source ----
    /// A name that is not the compiler's to know: a user function (mangled
    /// `__m<N>__name` after loading, `name$..` once monomorphized), a variant
    /// constructor (`Case@Variant`), a local holding a function value — or,
    /// before the loader has resolved imports, any spelling at all.
    User(String),
}

impl Callee {
    /// The reserved and synthesized arms — everything
    /// [`Callee::from_canonical`] can yield beyond the importable list above.
    const INTERNAL: &'static [Callee] = &[
        Callee::Some,
        Callee::Ok,
        Callee::Err,
        Callee::Assert,
        Callee::Trace,
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
        Callee::SplitIter,
        Callee::DictKeyAt,
        Callee::DictValueAt,
        Callee::WithOrSelf,
        Callee::RandomNext,
        Callee::RandomSeed,
        Callee::RandomSeedFromClock,
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
            Callee::CountIfIsLessThan => "__builtin_count_if_is_less_than",
            Callee::CountIfIsAtMost => "__builtin_count_if_is_at_most",
            Callee::CountIfIsGreaterThan => "__builtin_count_if_is_greater_than",
            Callee::CountIfIsAtLeast => "__builtin_count_if_is_at_least",
            Callee::CountIfIsEqual => "__builtin_count_if_is_equal",
            Callee::CountIfIsNotEqual => "__builtin_count_if_is_not_equal",
            Callee::First => "__builtin_first",
            Callee::GridGet => "__builtin_grid_get",
            Callee::GridFrom => "__builtin_grid_from",
            Callee::GridTrySet => "__builtin_grid_try_set",
            Callee::Width => "__builtin_width",
            Callee::SeededWithInt => "__builtin_seeded_with_int",
            Callee::SeededWithClock => "__builtin_seeded_with_clock",
            Callee::Height => "__builtin_height",
            Callee::IsInBoundsOf => "__builtin_is_in_bounds_of",
            Callee::GridIsInBoundsOf => "__builtin_grid_is_in_bounds_of",
            Callee::Last => "__builtin_last",
            Callee::NonemptyFirst => "__builtin_nonempty_first",
            Callee::NonemptyLast => "__builtin_nonempty_last",
            Callee::EnsureNonempty => "__builtin_ensure_nonempty",
            Callee::AssumeNonempty => "__builtin_assume_nonempty",
            Callee::DropFirst => "__builtin_drop_first",
            Callee::DropLast => "__builtin_drop_last",
            Callee::DropN => "__builtin_drop_n",
            Callee::DropLastN => "__builtin_drop_last_n",
            Callee::ToStr => "__builtin_to_str",
            Callee::ToStrLen => "__builtin_to_str_len",
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
            Callee::HeapStr => "__builtin_heap_str",
            Callee::RopeStr => "__builtin_rope_str",
            Callee::InlineStr => "__builtin_inline_str",
            Callee::PackInline => "__builtin_pack_inline",
            Callee::InlineCapacity => "__builtin_inline_capacity",
            Callee::Measure => "__builtin_measure",
            Callee::MeasureBegin => "__builtin_measure_begin",
            Callee::MeasureConsume => "__builtin_measure_consume",
            Callee::MeasureEnd => "__builtin_measure_end",
            Callee::MeasureRead => "__builtin_measure_read",
            Callee::IsAllWhitespace => "__builtin_is_all_whitespace",
            Callee::StartsWith(_) => "__builtin_starts_with",
            Callee::StartsWithAt(_) => "__builtin_starts_with_at",
            Callee::EndsWith(_) => "__builtin_ends_with",
            Callee::Contains(_) => "__builtin_contains",
            Callee::Len => "__builtin_len",
            Callee::Range => "__builtin_range",
            Callee::IsNonempty => "__builtin_is_nonempty",
            Callee::IsEmpty => "__builtin_is_empty",
            Callee::Push => "__builtin_push",
            Callee::Extend => "__builtin_extend",
            Callee::ExtendOptional => "__builtin_extend_optional",
            Callee::Reserve => "__builtin_reserve",
            Callee::IsSome => "__builtin_is_some",
            Callee::IsSomeAnd => "__builtin_is_some_and",
            Callee::IsErrAnd => "__builtin_is_err_and",
            Callee::MapErr => "__builtin_map_err",
            Callee::MapOk => "__builtin_map_ok",
            Callee::IntParse => "__builtin_int_parse",
            Callee::IntParseUnsigned => "__builtin_int_parse_unsigned",
            Callee::IsSpace => "__builtin_is_space",
            Callee::IsWhitespace => "__builtin_is_whitespace",
            Callee::IsDigit => "__builtin_is_digit",
            Callee::ToDigit => "__builtin_to_digit",
            Callee::Narrow => "__builtin_narrow",
            Callee::TrimWhile => "__builtin_trim_while",
            Callee::Words => "__builtin_words",
            Callee::CartesianProduct => "__builtin_cartesian_product",
            Callee::ClearScreen => "__builtin_clear_screen",
            Callee::Count => "__builtin_count",
            Callee::CountWhile => "__builtin_count_while",
            Callee::CountIf => "__builtin_count_if",
            Callee::FindIf => "__builtin_find_if",
            Callee::MapFindIf => "__builtin_map_find_if",
            Callee::MapJoin => "__builtin_map_join",
            Callee::JoinIsNonempty => "__builtin_join_is_nonempty",
            Callee::JoinIsEmpty => "__builtin_join_is_empty",
            Callee::Reduce => "__builtin_reduce",
            Callee::MapReduce => "__builtin_map_reduce",
            Callee::FilterMapReduce => "__builtin_filter_map_reduce",
            Callee::SplitMap => "__builtin_split_map",
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
            Callee::ReadLine => "__builtin_read_line",
            Callee::ReadByte => "__builtin_read_byte",
            Callee::ReadKey => "__builtin_read_key",
            Callee::RandomU64 => "__builtin_random_u64",
            Callee::RandomNext => "__random_next",
            Callee::RandomSeed => "__random_seed",
            Callee::RandomSeedFromClock => "__random_seed_from_clock",
            Callee::NowNanos => "__builtin_now_nanos",
            Callee::MonotonicNow => "__builtin_monotonic_now",
            Callee::ExecuteProgram => "__builtin_execute_program",
            Callee::Union => "__builtin_union",
            Callee::UnionAll => "__builtin_union_all",
            Callee::SetMap => "__builtin_set_map",
            Callee::Get => "__builtin_get",
            Callee::ContainsKey => "__builtin_contains_key",
            Callee::RemoveKey => "__builtin_remove_key",
            Callee::InsertKey => "__builtin_insert_key",
            Callee::ToDict => "__builtin_to_dict",
            Callee::MapToDict => "__builtin_map_to_dict",
            Callee::Hash => "__builtin_hash",
            Callee::Min => "__builtin_min",
            Callee::Max => "__builtin_max",
            Callee::Minimum => "__builtin_minimum",
            Callee::Maximum => "__builtin_maximum",
            Callee::Reverse => "__builtin_reverse",
            Callee::Sort => "__builtin_sort",
            Callee::SortBy => "__builtin_sort_by",
            Callee::Repeat => "__builtin_repeat",
            Callee::With => "__builtin_with",
            Callee::TrySet => "__builtin_try_set",
            Callee::WithOrSelf => "__with_or_self",

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
            Callee::Trace => "__trace",
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
            Callee::SplitIter => "__split_iter",
            Callee::DictKeyAt => "__dict_key_at",
            Callee::DictValueAt => "__dict_value_at",

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
        Builtin::all()
            .iter()
            .map(|b| &b.callee)
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
        Builtin::named(name).map(|b| b.callee.clone())
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
    /// builtin, operator impls included (or the reserved-prefix `with_capacity`
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

    /// Whether this builtin's result has exactly as many elements as its
    /// receiver, so the receiver's length answers for it and the result need
    /// never be built to be measured.
    ///
    /// Two places read this and have to agree. [`crate::fold_lengths`] answers
    /// `len(xs.map(f))` with `len(xs)`; the loader's array-spread desugaring
    /// needs a spread's length *without* evaluating the spread, because it
    /// needs that length to size one allocation and the operand itself to fill
    /// it (`desugar_spread`). Whichever asks, the fact is the same one, so it
    /// is stated once.
    ///
    /// A builtin that may *change* the count is deliberately absent —
    /// `filter`, `intersperse`, `split`. `map` can be here only because it has
    /// no set form: `set_map` is the separate name for a set receiver, whose
    /// result may be smaller when two elements map alike, and that separation
    /// is what lets `map` promise one element out per element in.
    ///
    /// It says nothing about *evaluating* the call. A caller that drops the
    /// remaining arguments' per-element calls rather than merely skipping the
    /// result has its own question to ask about them.
    pub fn preserves_length(&self) -> bool {
        matches!(self, Callee::Map | Callee::Sort | Callee::Reverse)
    }

    /// The builtin that combines two values into the one this builtin keeps, if
    /// this builtin is a *reduction*; `None` for every other arm.
    ///
    /// A reduction takes a sequence and returns one value built from all of it,
    /// or `none` when there is nothing to build from: it seeds with the first
    /// element and folds each following one in, in order, with a binary step —
    /// `minimum` is the fold of `min`, `maximum` of `max`. That step is what
    /// distinguishes one reduction from another and nothing else does, so a
    /// reduction is stated by naming it, and everything that treats reductions
    /// alike reads it from here.
    ///
    /// The contract a row owes is the one in that definition: the step must agree
    /// with the reduction on any two elements, so that
    /// `xs.reduction()` and "fold `xs` with `step`, seeded by the first element"
    /// are the same value. It is the only thing a pass that rewrites a reduction
    /// as a fold relies on. [`crate::fuse`] is the first such pass: a reduction fed
    /// by a derived sequence becomes one call that derives and folds in a single
    /// pass, taking the step as an argument.
    ///
    /// That is also all a reduction needs to be *evaluated*: it has no
    /// implementation of its own. The compiler lowers `xs.minimum()` to
    /// `xs.reduce(min)` (`lower_reductions` in `aipl-mono`), so `reduce` is the one
    /// implementation of every reduction and the step is the only code that
    /// differs. Adding a reduction is a line here and nothing else.
    pub fn reduction_step(&self) -> Option<Callee> {
        match self {
            Callee::Minimum => Some(Callee::Min),
            Callee::Maximum => Some(Callee::Max),
            _ => None,
        }
    }

    /// Whether this builtin is a reduction — see [`Callee::reduction_step`].
    pub fn is_reduction(&self) -> bool {
        self.reduction_step().is_some()
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
        for c in Builtin::all()
            .iter()
            .map(|b| &b.callee)
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

    /// A reduction's step has to be a builtin a program can name — the fused
    /// call that carries it as an argument is spelled with its canonical name.
    /// Nothing but a reduction has one.
    #[test]
    fn a_reductions_step_is_a_known_builtin() {
        assert_eq!(Callee::Minimum.reduction_step(), Some(Callee::Min));
        assert_eq!(Callee::Maximum.reduction_step(), Some(Callee::Max));
        assert!(!Callee::Map.is_reduction());
        assert!(!Callee::Len.is_reduction());
        assert!(!Callee::User("minimum".into()).is_reduction());
        for c in Builtin::all()
            .iter()
            .map(|b| &b.callee)
            .chain(Callee::INTERNAL)
        {
            if let Some(step) = c.reduction_step() {
                assert!(
                    Builtin::of(&step).is_some(),
                    "{c}'s step {step} is not an importable builtin"
                );
            }
        }
    }

    #[test]
    fn canonical_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in Builtin::all()
            .iter()
            .map(|b| &b.callee)
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
        // An operator impl is importable by name like any other builtin.
        assert_eq!(
            Callee::importable("wrapping_add"),
            Some(Callee::WrappingAdd)
        );
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
