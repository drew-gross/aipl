//! Signatures and documentation for the importable builtins that have no AIPL
//! source of their own — the ones the compiler expands or generates directly.
//!
//! An AIPL-implemented builtin (see [`crate::aipl_builtins`]) documents itself:
//! its file holds a `pub fn` with a `# ..` block, and a tool reads both from
//! there. The builtins listed here have no such file, so this table is the only
//! place their signature and prose exist, and it is what an editor shows on
//! hover. The signatures are written in AIPL's own declaration syntax so they
//! read like the ones from the other kind; they are descriptions, not
//! declarations the compiler checks — the rules each builtin is typed by live
//! in the checker.
//!
//! The table is keyed by the name an import list spells (`len`, not
//! `__builtin_len`). A test requires an entry for every importable builtin that
//! is not implemented in AIPL, and none for one that is, so a builtin is
//! documented in exactly one place.

/// One builtin's signature and documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinDoc {
    /// The name an import list spells, e.g. `len` or `wrapping_add`.
    pub name: &'static str,
    /// The signature on one line, as a declaration: `fn len<T: any>(self: T[]) -> u64`.
    pub signature: &'static str,
    /// What the builtin does, as the prose a `# ..` block would hold.
    pub doc: &'static str,
}

/// The documentation of the builtin an import list spells `name`, if it is
/// one of those without an AIPL source.
pub fn builtin_doc(name: &str) -> Option<&'static BuiltinDoc> {
    DOCS.iter().find(|d| d.name == name)
}

const fn doc(name: &'static str, signature: &'static str, doc: &'static str) -> BuiltinDoc {
    BuiltinDoc {
        name,
        signature,
        doc,
    }
}

const DOCS: &[BuiltinDoc] = &[
    // Text.
    doc(
        "print",
        "fn print(value: str) !prints",
        "Writes `value` and a newline to standard output.",
    ),
    doc(
        "split",
        "fn split(self: str, separator: str) -> str[]",
        "The pieces of `self` between occurrences of `separator`.",
    ),
    doc(
        "split_len",
        "fn split_len(self: str, separator: str) -> u64",
        "How many pieces `split` returns, without building them.",
    ),
    doc(
        "join",
        "fn join(self: str[], separator: str) -> str",
        "The elements, one after another, with `separator` between each pair.",
    ),
    doc(
        "to_str",
        "fn to_str<T: any>(self: T) -> str",
        "The text of a value, exactly what `` `{value}` `` interpolates: a `char` or `str` \
         renders bare, and one nested in an array, struct or optional keeps its literal form.",
    ),
    doc(
        "to_str_len",
        "fn to_str_len<T: any>(self: T) -> u64",
        "The length in bytes of `to_str(self)`, without building the text.",
    ),
    doc(
        "trim",
        "fn trim(self: str) -> str",
        "`self` without the whitespace at either end.",
    ),
    doc(
        "starts_with",
        "fn starts_with<T: any>(self: T[], prefix: T[]) -> bool",
        "Whether `self` begins with `prefix`. Also works on a `str`.",
    ),
    doc(
        "starts_with_at",
        "fn starts_with_at<T: any>(self: T[], prefix: T[], offset: u64) -> bool",
        "Whether `prefix` appears in `self` beginning at `offset`. An offset of 0 is \
         `starts_with`. Also works on a `str`.",
    ),
    doc(
        "ends_with",
        "fn ends_with<T: any>(self: T[], suffix: T[]) -> bool",
        "Whether `self` ends with `suffix`. Also works on a `str`.",
    ),
    doc(
        "heap_str",
        "fn heap_str(self: str) -> str",
        "The same text in a heap buffer, even when it would fit inline. For a test that \
         wants that representation by name.",
    ),
    doc(
        "rope_str",
        "fn rope_str(self: str) -> str",
        "The same text as a concatenation of pieces. For a test that wants that \
         representation by name.",
    ),
    doc(
        "pack_inline",
        "fn pack_inline(self: str) -> str",
        "The text packed into the inline representation. The caller must have checked it \
         fits in `inline_capacity()`.",
    ),
    doc(
        "inline_capacity",
        "fn inline_capacity() -> u64",
        "The longest text, in bytes, that the inline representation can hold.",
    ),
    // Characters.
    doc(
        "is_space",
        "fn is_space(self: char) -> bool",
        "Whether the character is a space.",
    ),
    doc(
        "is_whitespace",
        "fn is_whitespace(self: char) -> bool",
        "Whether the character is whitespace: a space, tab or line break.",
    ),
    doc(
        "is_digit",
        "fn is_digit(self: char) -> bool",
        "Whether the character is a decimal digit.",
    ),
    doc(
        "to_digit",
        "fn to_digit(self: char) -> u64?",
        "The value of a decimal digit, or `none` for any other character.",
    ),
    // Sequences.
    doc(
        "len",
        "fn len<T: any>(self: T[]) -> u64",
        "The number of elements. Also works on a `str` (its length in bytes), a set and \
         a dictionary.",
    ),
    doc(
        "is_empty",
        "fn is_empty<T: any>(self: T[]) -> bool",
        "Whether there are no elements.",
    ),
    doc(
        "is_nonempty",
        "fn is_nonempty<T: any>(self: T[]) -> bool",
        "Whether there is at least one element.",
    ),
    doc(
        "assume_nonempty",
        "fn assume_nonempty<T: any>(self: T[]) -> T[] without []",
        "Treats the array as nonempty without checking. The caller must already know it \
         is, as from an `is_nonempty` test.",
    ),
    doc(
        "push",
        "fn push<T: any>(self: T[], element: T) -> T[]",
        "The array with `element` added at the end. `set xs.push(x);` appends in place.",
    ),
    doc(
        "extend",
        "fn extend<T: any>(self: T[], other: T[]) -> T[]",
        "The array with the elements of `other` added at the end.",
    ),
    doc(
        "reserve",
        "fn reserve<T: any>(self: T[], additional: u64) -> T[]",
        "The array with room for `additional` more elements, so pushing them does not \
         reallocate.",
    ),
    doc(
        "map",
        "fn map<T: any, U: any>(self: T[], f: (T) -> U) -> U[]",
        "The array of `f` applied to each element, in order.",
    ),
    doc(
        "filter",
        "fn filter<T: any>(self: T[], keep: (T) -> bool) -> T[]",
        "The elements for which `keep` is true, in order.",
    ),
    doc(
        "filter_map",
        "fn filter_map<T: any, U: any>(self: T[], keep: (T) -> bool, f: (T) -> U) -> U[]",
        "`f` applied to each element for which `keep` is true.",
    ),
    doc(
        "zip_with",
        "fn zip_with<T: any, U: any, R: any>(self: T[], other: U[], f: (T, U) -> R) -> R[]",
        "`f` applied to the elements of both arrays pairwise. It stops at the shorter one.",
    ),
    doc(
        "contains",
        "fn contains<T: any>(self: T[], needle: T) -> bool",
        "Whether `needle` is an element. Also works on a `str` (a substring) and a set.",
    ),
    doc(
        "has",
        "fn has<T: any>(self: #{T}, element: T) -> bool",
        "Whether the set contains `element`.",
    ),
    doc(
        "to_array",
        "fn to_array<T: any>(self: #{T}) -> T[]",
        "The elements of a set or the entries of a dictionary, as an array.",
    ),
    doc(
        "to_set",
        "fn to_set<T: any>(self: T[]) -> #{T}",
        "The distinct elements as a set. The element type comes from the context.",
    ),
    doc(
        "reverse",
        "fn reverse<T: any>(self: T[]) -> T[]",
        "The elements in the opposite order.",
    ),
    doc(
        "sort",
        "fn sort<T: ord>(self: T[]) -> T[]",
        "The elements in ascending order.",
    ),
    doc(
        "minimum",
        "fn minimum<T: ord>(self: T[]) -> T?",
        "The smallest element, or `none` if the array is empty.",
    ),
    doc(
        "maximum",
        "fn maximum<T: ord>(self: T[]) -> T?",
        "The largest element, or `none` if the array is empty.",
    ),
    doc(
        "with",
        "fn with<T: any>(self: T[], index: u64, element: T) -> T[]?",
        "The array with the element at `index` replaced, or `none` if `index` is out of \
         bounds.",
    ),
    doc(
        "try_set",
        "fn try_set<T: any>(self: T[], index: u64, element: T) -> T[]",
        "The array with the element at `index` replaced, or the array unchanged if \
         `index` is out of bounds.",
    ),
    // Sets and dictionaries.
    doc(
        "union",
        "fn union<T: any>(self: #{T}, other: #{T}) -> #{T}",
        "The elements of either set.",
    ),
    doc(
        "get",
        "fn get<K: any, V: any>(self: {K: V}, key: K) -> V?",
        "The value stored under `key`, or `none` if there is none.",
    ),
    doc(
        "contains_key",
        "fn contains_key<K: any, V: any>(self: {K: V}, key: K) -> bool",
        "Whether a value is stored under `key`.",
    ),
    doc(
        "remove_key",
        "fn remove_key<K: any, V: any>(self: {K: V}, key: K) -> {K: V}",
        "The dictionary without `key`. `set d.remove_key(k);` removes in place.",
    ),
    doc(
        "hash",
        "fn hash<T: any>(self: T) -> u64",
        "A hash of the value. Equal values hash equal, and it is the same on every run.",
    ),
    // Optionals and variants.
    doc(
        "is_some",
        "fn is_some<T: any>(self: T?) -> bool",
        "Whether the optional holds a value.",
    ),
    doc(
        "case_name",
        "fn case_name<K: variant>(self: K) -> str",
        "The name of the case the value is, e.g. `\"Circle\"`.",
    ),
    doc(
        "case_of",
        "fn case_of<K: variant>(self: K) -> Case<K>",
        "Which case the value is, without its payload. Cases compare with `==`.",
    ),
    doc(
        "same_case",
        "fn same_case<K: variant>(self: K, other: K) -> bool",
        "Whether both values are the same case, whatever their payloads.",
    ),
    // Numbers.
    doc(
        "narrow",
        "fn narrow<T: int, U: int>(self: T) -> U?",
        "Converts to a narrower integer type, or `none` when the value does not fit. The \
         target type comes from the context.",
    ),
    doc(
        "min",
        "fn min<T: ord>(a: T, b: T) -> T",
        "The smaller of two values.",
    ),
    doc(
        "max",
        "fn max<T: ord>(a: T, b: T) -> T",
        "The larger of two values.",
    ),
    // Operators. Each is imported aliased: `wrapping_add as +`.
    doc(
        "wrapping_add",
        "fn wrapping_add<T: int>(a: T, b: T) -> T",
        "Addition that wraps around on overflow. Import it as the operator: \
         `wrapping_add as +`.",
    ),
    doc(
        "saturating_add",
        "fn saturating_add<T: int>(a: T, b: T) -> T",
        "Addition that stops at the type's largest value on overflow. Import it as the \
         operator: `saturating_add as +`.",
    ),
    doc(
        "wrapping_sub",
        "fn wrapping_sub<T: int>(a: T, b: T) -> T",
        "Subtraction that wraps around on overflow. Import it as the operator: \
         `wrapping_sub as -`.",
    ),
    doc(
        "saturating_sub",
        "fn saturating_sub<T: int>(a: T, b: T) -> T",
        "Subtraction that stops at the type's smallest value on overflow. Import it as the \
         operator: `saturating_sub as -`.",
    ),
    doc(
        "wrapping_mul",
        "fn wrapping_mul<T: int>(a: T, b: T) -> T",
        "Multiplication that wraps around on overflow. Import it as the operator: \
         `wrapping_mul as *`.",
    ),
    doc(
        "saturating_divide",
        "fn saturating_divide<T: int>(a: T, b: T) -> T",
        "Division that never traps: it saturates where the result would not fit. Import it \
         as the operator: `saturating_divide as /`.",
    ),
    doc(
        "saturating_remainder",
        "fn saturating_remainder<T: int>(a: T, b: T) -> T",
        "The remainder of division, never trapping. Import it as the operator: \
         `saturating_remainder as %`.",
    ),
    doc(
        "equal",
        "fn equal<T: any>(a: T, b: T) -> bool",
        "Whether the two values are equal. Import it as the operator: `equal as ==`.",
    ),
    doc(
        "not_equal",
        "fn not_equal<T: any>(a: T, b: T) -> bool",
        "Whether the two values differ. Import it as the operator: `not_equal as !=`.",
    ),
    doc(
        "less_than",
        "fn less_than<T: ord>(a: T, b: T) -> bool",
        "Whether `a` is less than `b`. Import it as the operator: `less_than as <`.",
    ),
    doc(
        "greater_than",
        "fn greater_than<T: ord>(a: T, b: T) -> bool",
        "Whether `a` is greater than `b`. Import it as the operator: `greater_than as >`.",
    ),
    doc(
        "less_than_or_equal",
        "fn less_than_or_equal<T: ord>(a: T, b: T) -> bool",
        "Whether `a` is less than or equal to `b`. Import it as the operator: \
         `less_than_or_equal as <=`.",
    ),
    doc(
        "greater_than_or_equal",
        "fn greater_than_or_equal<T: ord>(a: T, b: T) -> bool",
        "Whether `a` is greater than or equal to `b`. Import it as the operator: \
         `greater_than_or_equal as >=`.",
    ),
    doc(
        "logical_and",
        "fn logical_and(a: bool, b: bool) -> bool",
        "Whether both are true; `b` is not evaluated when `a` is false. Import it as the \
         operator: `logical_and as &&`.",
    ),
    doc(
        "logical_or",
        "fn logical_or(a: bool, b: bool) -> bool",
        "Whether either is true; `b` is not evaluated when `a` is true. Import it as the \
         operator: `logical_or as ||`.",
    ),
    doc(
        "logical_not",
        "fn logical_not(a: bool) -> bool",
        "Whether `a` is false. Import it as the operator: `logical_not as !`.",
    ),
    doc(
        "concat",
        "fn concat<T: any>(a: T[], b: T[]) -> T[]",
        "The elements of `a` followed by those of `b`. Also joins two `str`s. Import it as \
         the operator: `concat as +++`.",
    ),
    // Effects. Each needs its effect declared by the function that calls it.
    doc(
        "read_file_to_string",
        "fn read_file_to_string(path: str) !reads_files -> str!Error",
        "The contents of the file, or the error that stopped it being read.",
    ),
    doc(
        "write_string_to_file",
        "fn write_string_to_file(path: str, contents: str) !writes_files -> ()!Error",
        "Writes `contents` to the file, replacing what was there.",
    ),
    doc(
        "list_files",
        "fn list_files(directory: str) !list_files -> str[]!Error",
        "The names of the entries in the directory.",
    ),
    doc(
        "read_line",
        "fn read_line() !reads_stdin -> str?",
        "The next line of standard input, or `none` at the end of it.",
    ),
    doc(
        "random_u64",
        "fn random_u64() !random -> u64",
        "A random 64-bit integer. The `random` effect has no default implementation: a \
         `shim random` must stand in for it.",
    ),
    doc(
        "now_nanos",
        "fn now_nanos() !clock -> u64",
        "The wall-clock time in nanoseconds since the Unix epoch.",
    ),
    doc(
        "monotonic_now",
        "fn monotonic_now() !clock -> u64",
        "A time in nanoseconds that only moves forward, for measuring durations.",
    ),
    doc(
        "execute_program",
        "fn execute_program(program: str, args: str[] = []) !runs_programs -> ProgramOutput!Error",
        "Runs a program to completion and returns what it wrote and how it exited.",
    ),
    // Measurement. The machinery `measure` is written with.
    doc(
        "measure_begin",
        "fn measure_begin() !measure",
        "Opens the counting window. Internal to `measure`.",
    ),
    doc(
        "measure_consume",
        "fn measure_consume<T: any>(value: T) !measure",
        "Releases `value` inside the counting window. Internal to `measure`.",
    ),
    doc(
        "measure_end",
        "fn measure_end() !measure",
        "Closes the counting window. Internal to `measure`.",
    ),
    doc(
        "measure_read",
        "fn measure_read() !measure -> Measurement",
        "The counts of the window just closed. Internal to `measure`.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aipl_builtins::is_aipl_implemented;
    use crate::ast::Callee;

    #[test]
    fn every_builtin_without_an_aipl_source_is_documented_here() {
        let missing: Vec<&str> = Callee::IMPORTABLE
            .iter()
            .filter(|c| !is_aipl_implemented(c))
            .map(|c| c.import_name())
            .filter(|name| builtin_doc(name).is_none())
            .collect();
        assert!(missing.is_empty(), "undocumented builtins: {missing:?}");
    }

    #[test]
    fn nothing_here_documents_a_builtin_that_has_its_own_source() {
        let doubled: Vec<&str> = DOCS
            .iter()
            .map(|d| d.name)
            .filter(|name| Callee::importable(name).is_some_and(|c| is_aipl_implemented(&c)))
            .collect();
        assert!(doubled.is_empty(), "documented twice: {doubled:?}");
    }

    #[test]
    fn every_entry_names_an_importable_builtin_once() {
        for (i, d) in DOCS.iter().enumerate() {
            assert!(
                Callee::importable(d.name).is_some(),
                "`{}` is not an importable builtin",
                d.name
            );
            assert!(
                DOCS[..i].iter().all(|earlier| earlier.name != d.name),
                "`{}` is documented twice",
                d.name
            );
        }
    }

    #[test]
    fn a_signature_declares_the_function_it_documents() {
        for d in DOCS {
            assert!(
                d.signature.starts_with(&format!("fn {}", d.name)),
                "`{}`: signature `{}`",
                d.name,
                d.signature
            );
        }
    }
}
