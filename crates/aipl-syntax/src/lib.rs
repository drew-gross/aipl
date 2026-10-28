//! AIPL core types shared across every compiler crate: source spans, the
//! [`Error`] type, [`DebugOptions`] tracing, the [`ast`] tree, and the
//! AST-level `Type`/builtin helpers the parser, monomorphizer, codegen, and
//! loader all need. This crate has no external dependencies, so it forms the
//! fast-to-compile base of the workspace.

/// Byte-offset range in the source string.
pub type Span = std::ops::Range<usize>;

/// Matched text together with where it was matched — the Rust twin of
/// `lexer.aipl`'s `SpanStr`, and what a declaration carries for its own name.
///
/// The two halves are one value because every consumer of a name wants both and
/// neither is derivable on its own: a diagnostic renders `span` against the
/// source it already holds, and everything else reads `text` without needing the
/// source at all. Keeping them together is also what retired the token-stream
/// walk `aipl-index` used to do to recover a declared name's position — the AST
/// simply carries it now.
///
/// **Its invariant is `text == src[span]`** for the source it was built from.
/// Nothing enforces that here, because the AST is built by the dogfooded parser
/// (which builds every one through `lexer.aipl`'s `span_str`, the only
/// constructor there); this side only mirrors the result across the FFI.
///
/// A hand-written twin rather than a marshaled foreign type on purpose: when
/// codegen self-hosts, the AST *becomes* the AIPL one and this goes away.
/// `Deref<Target = str>` is what keeps the change from rippling: a name reads as
/// a `&str` wherever it always did.
#[derive(Clone, Default)]
pub struct SpanStr {
    pub text: String,
    pub span: Span,
}

/// Formats as the *text* would, quotes included — so a diagnostic that writes a
/// name with `{:?}` reads as it always did (`fn "add": body returns ..`) rather
/// than printing the struct around it. Dozens of messages across the checker,
/// mono and the lints do exactly that.
///
/// Hiding a field from `Debug` is usually a trap, and it is safe here only
/// because [`PartialEq`](SpanStr#impl-PartialEq) ignores the span too: two
/// `SpanStr`s that compare unequal differ in their text, which is what this
/// prints. Reach for `.span` when the position is what you want.
impl std::fmt::Debug for SpanStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.text, f)
    }
}

/// Compares the *text* and ignores the span, exactly as [`ast::Expr`]'s does and
/// for the same reason: a span says where a name sits, not what it says, and
/// reformatting a source moves every span in it without changing the program.
/// Tests that parse two spellings of one program and compare the trees rely on
/// that — `parser::whitespace_is_irrelevant` is the one that names it.
impl PartialEq for SpanStr {
    fn eq(&self, other: &SpanStr) -> bool {
        self.text == other.text
    }
}

impl Eq for SpanStr {}

impl SpanStr {
    pub fn new(text: impl Into<String>, span: Span) -> SpanStr {
        SpanStr {
            text: text.into(),
            span,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The same name spelled differently, keeping its span: what a rename gives.
    ///
    /// The loader mangles every top-level name to a global one, and the mangled
    /// spelling is written nowhere — so the span it keeps is the one the author
    /// *did* write, which is where a diagnostic about the declaration belongs.
    pub fn renamed(&self, text: impl Into<String>) -> SpanStr {
        SpanStr::new(text, self.span.clone())
    }

    /// A name with no position — for a declaration a *pass* synthesized, which
    /// was never written anywhere and so has no source to point at. An empty
    /// span is the codebase's "no location" placeholder; see [`join_spans`],
    /// which deliberately ignores one.
    pub fn synthetic(text: impl Into<String>) -> SpanStr {
        SpanStr::new(text, 0..0)
    }
}

impl std::ops::Deref for SpanStr {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for SpanStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq<str> for SpanStr {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<String> for SpanStr {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl PartialEq<SpanStr> for String {
    fn eq(&self, other: &SpanStr) -> bool {
        *self == other.text
    }
}

impl PartialEq<&String> for SpanStr {
    fn eq(&self, other: &&String) -> bool {
        self.text == **other
    }
}

impl PartialEq<&str> for SpanStr {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

/// Smallest span covering both `a` and `b`.
pub fn join_spans(a: &Span, b: &Span) -> Span {
    // An *empty* span is the "no location" placeholder — an empty block body
    // has no text of its own to point at. It must not take part in a join:
    // `0..0` would drag the result back to offset 0 and make every enclosing
    // construct appear to start at the top of the file. Contributing nothing is
    // the honest answer, and leaves the other operand as the whole location.
    if a.is_empty() {
        return b.clone();
    }
    if b.is_empty() {
        return a.clone();
    }
    a.start.min(b.start)..a.end.max(b.end)
}

/// One error from parsing, checking, or codegen. Use [`Error::render`] for the
/// human-friendly rendering with source-line context.
///
/// A pass that can keep going after a failure (the checker, which recovers at
/// each item; the lints, which are collected wholesale) reports *every*
/// independent finding, as a `Vec<Error>` propagated up to whoever prints —
/// see [`Error::render_all`]. That plurality lives in the signature rather than
/// inside `Error`, so a value of this type is always exactly one error, and the
/// deeply recursive checker and codegen keep passing the small
/// `Result<_, Error>` their frames are sized for. [`From<Error>`] lifts a single
/// error into that `Vec`, so `?` still carries a leaf error up into a
/// multi-error signature unchanged.
#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub span: Option<Span>,
    /// Secondary labeled locations, rendered as `note:` blocks after the
    /// primary caret — e.g. pointing at the *other* side of a conflict.
    pub notes: Vec<(String, Span)>,
    /// The file this error's spans index, when that isn't the file the
    /// renderer was handed. The loader sets it on every diagnostic raised
    /// against an *imported* file: those spans point into that file's source,
    /// while the caller renders against the entry file's, so without this the
    /// caret lands on an unrelated line of the wrong file. Boxed to keep
    /// `Error` small on the common (unset) path.
    pub origin: Option<Box<ErrorOrigin>>,
}

/// The source an [`Error`]'s spans belong to, carried when it isn't the source
/// the renderer is given. `label` names the file in the ` --> ` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorOrigin {
    pub label: String,
    pub source: String,
}

/// Lift a single error into the multi-error form the pipeline propagates, so a
/// leaf `Result<_, Error>` flows through `?` into a `Result<_, Vec<Error>>`
/// without a per-call-site `vec![..]`.
impl From<Error> for Vec<Error> {
    fn from(e: Error) -> Self {
        vec![e]
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.span {
            Some(s) => write!(f, "{} (at bytes {}..{})", self.message, s.start, s.end),
            None => f.write_str(&self.message),
        }
    }
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
            notes: Vec::new(),
            origin: None,
        }
    }

    pub fn at(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span: Some(span),
            notes: Vec::new(),
            origin: None,
        }
    }

    /// This error, located at `span` if it has no location of its own. For a
    /// check that knows only which declaration it was running on — a type in a
    /// signature carries no span of its own — so that a diagnostic points at
    /// the declaration rather than at nothing.
    pub fn or_at(mut self, span: Span) -> Self {
        if self.span.is_none() {
            self.span = Some(span);
        }
        self
    }

    /// Record that this error's spans index `source`, the file named `label`.
    /// The *innermost* attribution wins: an error bubbling out through a chain
    /// of imports is already tagged with the file it was raised against, and
    /// each enclosing file must leave that alone.
    pub fn in_file(mut self, label: impl Into<String>, source: &str) -> Self {
        if self.origin.is_none() {
            self.origin = Some(Box::new(ErrorOrigin {
                label: label.into(),
                source: source.to_string(),
            }));
        }
        self
    }

    /// Attach a secondary labeled location, rendered as a `note:` block after
    /// the primary caret. Chainable; notes render in the order added.
    pub fn with_note(mut self, message: impl Into<String>, span: Span) -> Self {
        self.notes.push((message.into(), span));
        self
    }

    /// Render this error against the source string. A primary caret pointer
    /// when a span is present (plus a `note:` block per secondary span), or a
    /// plain `error: ...` otherwise. `filename` appears in the ` --> ` location
    /// line; pass `"input"` when no real path is available.
    pub fn render(&self, source: &str, filename: &str) -> String {
        // A diagnostic carrying its own origin indexes *that* source, not the
        // one the caller happens to hold — see `Error::origin`.
        let (source, filename) = match &self.origin {
            Some(o) => (o.source.as_str(), o.label.as_str()),
            None => (source, filename),
        };
        let Some(span) = self.span.as_ref() else {
            return format!("error: {}", self.message);
        };
        let mut out = format!(
            "error: {}\n{}",
            self.message,
            caret_block(source, span, filename)
        );
        for (note, nspan) in &self.notes {
            out.push_str(&format!(
                "\nnote: {note}\n{}",
                caret_block(source, nspan, filename)
            ));
        }
        out
    }

    /// Render every independent finding a pass reported, in the order found —
    /// each a full [`Error::render`] block, one blank line apart. This is what a
    /// caller holding the `Vec<Error>` the pipeline propagates prints.
    pub fn render_all(errors: &[Error], source: &str, filename: &str) -> String {
        errors
            .iter()
            .map(|e| e.render(source, filename))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Every error's plain [`Display`] form, one per line — the sourceless
    /// counterpart to [`Error::render_all`], for a caller with no source text to
    /// point a caret into.
    ///
    /// [`Display`]: std::fmt::Display
    pub fn display_all(errors: &[Error]) -> String {
        errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The rustc-style location + caret block for a single span (no leading label
/// line — callers prepend `error:`/`note:`). Computed by the dogfooded AIPL
/// `caret_block` via the embedding FFI (see [`set_caret_block_hook`]).
fn caret_block(source: &str, span: &Span, filename: &str) -> String {
    CARET_BLOCK_HOOK.get().expect(
        "caret_block hook not installed before rendering an error \
         (call install_parser_hooks first)",
    )(source, span.clone(), filename)
}

/// Controls compiler debug output. Threaded through every pass so the
/// `--debug` CLI flag can trace progress to stderr: the last line printed
/// before a hang localizes an infinite loop to a specific pass — and, for
/// monomorphization, to the exact runaway generic instance.
#[derive(Debug, Clone, Copy, Default)]
pub struct DebugOptions {
    /// When set, each pass prints `[aipl-debug] ...` progress lines to stderr.
    pub enabled: bool,
}

impl DebugOptions {
    /// Tracing disabled — the default for library callers and tests.
    pub const OFF: DebugOptions = DebugOptions { enabled: false };

    /// Build options with tracing set to `enabled`.
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Emit one `[aipl-debug] <phase>: <message>` line to stderr when tracing
    /// is enabled. Pass the message as `format_args!(...)`: it is only
    /// rendered when tracing is on, so calls stay cheap on the hot path.
    pub fn trace(&self, phase: &str, args: std::fmt::Arguments<'_>) {
        if self.enabled {
            eprintln!("[aipl-debug] {phase}: {args}");
        }
    }
}

/// The hook called by [`caret_block`] to format the location + underline block
/// for a span. Installed by the compiler via [`set_caret_block_hook`] (the
/// dogfooded AIPL `caret_block`, run through the embedding FFI). No native
/// fallback — panics if not installed.
static CARET_BLOCK_HOOK: std::sync::OnceLock<fn(&str, Span, &str) -> String> =
    std::sync::OnceLock::new();

/// Install the caret-block hook (the dogfooded AIPL `caret_block`, run through
/// the embedding FFI). Idempotent — first install wins. Must be called before
/// any [`Error::render`] with a span (i.e. before `install_parser_hooks` returns).
pub fn set_caret_block_hook(f: fn(&str, Span, &str) -> String) {
    let _ = CARET_BLOCK_HOOK.set(f);
}

pub mod aipl_builtins;
pub mod builtin;
pub mod callee;
pub mod rename;
pub mod resolution;
pub use rename::{
    distinct_binders, fresh_id, fresh_local, fresh_temp, rename_params, rename_scoped,
};

pub mod ast {
    pub use crate::callee::{Callee, SeqShape};
    pub use crate::SpanStr;

    use crate::Span;

    /// One source file behind a merged program: the label diagnostics name it
    /// by, and the text its spans index. A file's position in
    /// [`Program::sources`] is the loader's file index — the `<N>` in a mangled
    /// `__m<N>__name` — so a pass holding an item's global name can recover the
    /// file it came from (see [`crate::mangled_file_index`]).
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct FileSource {
        pub label: String,
        pub source: String,
        /// The file's own path, as the loader read it — what `label` abbreviates
        /// for a person. Empty for a source that has no file.
        pub path: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Program {
        pub items: Vec<Item>,
        /// The files this program was flattened from, indexed by loader file
        /// index. Empty for a program that was never merged (a single parsed
        /// file, or one synthesized by a pass), in which case spans index
        /// whatever source the caller holds — which is what they did before this
        /// existed.
        ///
        /// This exists so a diagnostic raised by a *post-load* pass can be
        /// attributed to the file it belongs to. The loader tags the errors it
        /// raises itself (`tag_origin`), but everything after flattening runs on
        /// one merged program where per-file identity survives only in the
        /// mangled names — so without this a checker error against an imported
        /// file gets rendered into the entry file's source, at a byte offset
        /// that means nothing there.
        pub sources: Vec<FileSource>,
        /// The file's own documentation: the `# ..` block at the top that a
        /// blank line detaches from whatever follows, so it documents no
        /// declaration. `None` for a file that opens with anything else.
        ///
        /// A *file's*, so it survives only as long as the program is one file.
        /// A pass rebuilding a program carries it through, and the loader's
        /// merge drops it: a flattened program is several files, and the entry
        /// file's documentation is no more the merged program's than any
        /// other's. Nothing downstream of the loader reads it — this is for the
        /// tools that index one file at a time (`aipl doc`, `aipl docs`).
        pub doc: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Item {
        Fn(Function),
        Struct(StructDecl),
        Variant(VariantDecl),
        Refinement(RefinementDecl),
        Import(ImportDecl),
        Const(ConstDecl),
    }

    /// `variant Name = V without C` — a named refinement of a variant: the type
    /// `V without C`, spelled once and given a name.
    ///
    /// Like a constant, it is resolved by the loader and gone before anything
    /// else sees the program: every mention of `Name` is replaced by `target`
    /// (with the arguments of a generic one substituted), so the checker meets
    /// only the [`Type::Without`] it stands for. It is declared with `variant`
    /// because that is what it names — a variant with fewer cases — but it has
    /// no cases of its own and no representation of its own: a `Name` is a `V`
    /// at runtime.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RefinementDecl {
        pub name: SpanStr,
        /// The `# ..` doc comment above the declaration, lines joined with
        /// newlines. `None` when it carries none.
        pub doc: Option<String>,
        /// Declared generic type parameters, as on [`VariantDecl`]; `variant
        /// Rest<T> = Tree<T> without Leaf` names one refinement per `T`.
        pub type_vars: Vec<TypeParam>,
        /// The refined type — always a [`Type::Without`].
        pub target: Type,
    }

    /// A top-level constant: `pub let NAME: T = <value>;`.
    ///
    /// A literal needs no evaluation order and no initialization pass — the
    /// loader substitutes each use with the literal itself and drops the
    /// declaration. Any other value is computed once, on first use: the loader
    /// turns the declaration into a nullary function
    /// ([`constant_initializer_name`](crate::constant_initializer_name)) and each
    /// use into a call to it, and codegen answers those calls from a cache. Either
    /// way nothing past the loader knows constants exist. There is no `mut` form:
    /// a mutable one would need storage that uses could disagree about.
    ///
    /// `name` is ALL_CAPS by rule, which is what keeps a use distinguishable from
    /// an ordinary binding at the site that reads it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConstDecl {
        /// The name, with its own span — what the diagnostics checking a
        /// constant's two rules point at (ALL_CAPS, and a type on a computed value).
        pub name: SpanStr,
        /// Declared `pub`: importable by other files, like a `pub fn`.
        pub is_pub: bool,
        /// The written annotation. A literal types itself, so for one it is only
        /// ever a narrowing (`let PORT: u16 = 8080;`); any other value must have
        /// it, since it becomes the initializer's return type.
        pub ty: Option<Type>,
        /// The value this name stands for.
        pub value: Expr,
        /// The `# ..` doc comment above the declaration.
        pub doc: Option<String>,
    }

    /// `import { foo, bar as baz } from "./util.aipl";` — a request to pull a
    /// specific set of items into the current file's namespace. The loader
    /// resolves `from` relative to the importing file.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ImportDecl {
        pub names: Vec<ImportName>,
        pub source: ImportSource,
        /// The source as written (`"./util.aipl"` or `builtins`), where a
        /// diagnostic about the import as a whole points.
        pub source_span: Span,
    }

    /// One name in an `import { .. }` list: the exported `name`, optionally bound
    /// under a different local `alias` (`name as alias`). The `span` covers the
    /// imported name for diagnostics.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ImportName {
        pub name: String,
        pub alias: Option<String>,
        pub span: Span,
    }

    impl ImportName {
        /// The name this import binds in the importing file: the alias if given,
        /// else the exported name.
        pub fn local(&self) -> &str {
            self.alias.as_deref().unwrap_or(&self.name)
        }
    }

    /// Where an `import { .. } from <source>;` pulls its names from.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ImportSource {
        /// `from "path"` — another AIPL source file, resolved relative to
        /// the importing file.
        Path(String),
        /// `from builtins` — the built-in namespace (`print`, `len`, …).
        /// Every builtin must be imported before use, exactly like a
        /// user item, so user idents never silently shadow them.
        Builtins,
    }

    /// A constraint on a declared type variable, restricting which concrete
    /// types it may be instantiated with. `any` (the default almost every
    /// generic uses) accepts anything; `ord` narrows a variable to the
    /// primitives usable with `<`/`>` — currently the integers and `char` —
    /// so a signature like `minimum`/`maximum`'s can declare comparability
    /// itself instead of a caller hand-checking element types by name. `int`
    /// narrows further, to the integer primitives alone.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Bound {
        Any,
        Ord,
        Int,
        Variant,
    }

    impl Bound {
        /// Parse a bound keyword from source text (the `any`/`ord` in
        /// `<T: any>`/`<T: ord>`), or `None` if unrecognized.
        pub fn from_name(name: &str) -> Option<Bound> {
            match name {
                "any" => Some(Bound::Any),
                "ord" => Some(Bound::Ord),
                "int" => Some(Bound::Int),
                "variant" => Some(Bound::Variant),
                _ => None,
            }
        }

        /// Source spelling of this bound, for error messages.
        pub fn name(&self) -> &'static str {
            match self {
                Bound::Any => "any",
                Bound::Ord => "ord",
                Bound::Int => "int",
                Bound::Variant => "variant",
            }
        }

        /// Does `ty` satisfy this bound? `Any` accepts everything; `Ord`
        /// accepts only the primitives usable with `<`/`>` — integers and
        /// `char`; `Int` accepts only the integers.
        ///
        /// `Variant` accepts only a named variant type, which the `Type` alone
        /// cannot decide — `Type::Named("Foo")` is equally a struct — so the
        /// caller supplies `is_variant`, which answers it against the program's
        /// type table.
        pub fn accepts(&self, ty: &Type, is_variant: &dyn Fn(&str) -> bool) -> bool {
            match self {
                Bound::Any => true,
                Bound::Ord => matches!(
                    ty,
                    Type::Primitive(p)
                        if p.is_int() || *p == Primitive::Char || *p == Primitive::Str
                ),
                Bound::Int => matches!(ty, Type::Primitive(p) if p.is_int()),
                // A `Case<V>` satisfies it too: the bound exists so a body can
                // ask which case a value is, and a case is exactly that answer
                // with the payload dropped. `case_name` therefore reads one the
                // same way it reads a variant value, which is why it needs no
                // second signature.
                Bound::Variant => match ty {
                    Type::Named(n) => is_variant(n),
                    Type::Case(v) => match &**v {
                        Type::Named(n) => is_variant(n),
                        // Inside a generic body the variant is still a variable
                        // — and one that carries this very bound, since
                        // `Case<K>` only type-checks where `K` is a variant. So
                        // `k.case_name()` works in `match_term` as well as at a
                        // concrete instance.
                        Type::TypeVar(_) => true,
                        _ => false,
                    },
                    _ => false,
                },
            }
        }
    }

    /// A declared generic type parameter: its name and bound (see [`Bound`]).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TypeParam {
        pub name: String,
        pub bound: Bound,
        /// The type the parameter takes when a call leaves nothing to infer it from,
        /// written `<T: any = i64>`. Only a function's parameter has one.
        pub default: Option<Type>,
    }

    /// A function's shape apart from its body and source-only concerns (name,
    /// visibility, its `.test` block, its documentation): its declared type
    /// variables, value
    /// parameters, declared effects, and return type. Shared with aipl-mono,
    /// which normalizes its own copy of this (synthesizing a type variable per
    /// anonymous `any[]`/`any?` parameter, and rewriting those parameters to
    /// reference it) ahead of monomorphizing a generic — see
    /// `aipl_mono::normalize`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Signature {
        /// Declared generic type parameters, e.g. `fn f<T: any>(...)` →
        /// `[TypeParam { name: "T", bound: Any }]`. These names act as type
        /// variables in `params`/`return_ty` and are resolved by
        /// monomorphization.
        pub type_vars: Vec<TypeParam>,
        pub params: Vec<Param>,
        /// Effects declared in the signature, e.g. `!prints`. Callers of this
        /// function must declare at least these effects themselves.
        pub effects: Vec<String>,
        pub return_ty: Option<Type>,
    }

    impl Signature {
        /// Each parameter's declared type, discarding name/mutability/variadic.
        pub fn param_types(&self) -> Vec<Type> {
            self.params.iter().map(|p| p.ty.clone()).collect()
        }

        /// The default of each type parameter that declares one, by name.
        pub fn type_var_defaults(&self) -> std::collections::HashMap<String, Type> {
            self.type_vars
                .iter()
                .filter_map(|tp| tp.default.clone().map(|d| (tp.name.clone(), d)))
                .collect()
        }

        /// The declared return type, defaulting to `Unit` — a function with no
        /// `-> T` returns unit.
        pub fn return_type(&self) -> Type {
            self.return_ty.clone().unwrap_or(Type::Unit)
        }

        /// `true` for a mutating method (`fn f(mut self: T, ...)`). It is
        /// *declared* void, but a call yields the mutated receiver, so the
        /// effective return type is the receiver's — see
        /// `check::return_ty_of`. Written `set v.f(...)` it mutates `v` in
        /// place; in any other position (including the free call `f(v, ...)`)
        /// it is copy-and-modify, yielding a fresh value and leaving `v` alone.
        pub fn is_mutating(&self) -> bool {
            self.params.first().is_some_and(|p| p.mutable)
        }

        /// First parameter is named `self`, so it's callable as `recv.f(..)`.
        pub fn is_method(&self) -> bool {
            self.params.first().is_some_and(|p| p.name == "self")
        }

        /// A function is generic if it declares type parameters or uses
        /// anonymous `any` in a parameter.
        pub fn is_generic(&self) -> bool {
            !self.type_vars.is_empty() || self.params.iter().any(|p| ty_mentions_any(&p.ty))
        }

        /// Just the declared type variables' names, discarding their bounds —
        /// for callers that only need to recognize which `Type::Named` values
        /// are in-scope type parameters (substitution, name-validity checks).
        pub fn type_var_names(&self) -> Vec<String> {
            self.type_vars.iter().map(|tp| tp.name.clone()).collect()
        }
    }

    fn ty_mentions_any(t: &Type) -> bool {
        match t {
            Type::Case(v) => ty_mentions_any(v),
            Type::Unit
            | Type::Primitive(_)
            | Type::Named(_)
            // A variable `any` already normalized into is no longer an `any`.
            | Type::TypeVar(_)
            | Type::NoneInner
            | Type::EmptyArrayArg
            | Type::NoneLiteralArg
            | Type::Unknown
            | Type::ConcatStr => false,
            Type::Any => true,
            Type::Array(inner)
            | Type::Optional(inner)
            | Type::Set(inner, _)
            | Type::Without(inner, _) => ty_mentions_any(inner),
            Type::Dict(k, v) => ty_mentions_any(k) || ty_mentions_any(v),
            Type::Result(ok, err) => ty_mentions_any(ok) || ty_mentions_any(err),
            Type::Fn(params, ret) => params.iter().any(ty_mentions_any) || ty_mentions_any(ret),
            Type::Tuple(elems) => elems.iter().any(ty_mentions_any),
            Type::Generic(_, args) => args.iter().any(ty_mentions_any),
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Function {
        pub name: SpanStr,
        /// Declared `pub`: the function may be imported by other files. A
        /// non-`pub` (private) function is usable only within its own file —
        /// importing it is a loader error. Always treated as public for the
        /// builtin pseudo-declarations and within a single file.
        pub is_pub: bool,
        pub sig: Signature,
        pub body: Expr,
        /// The body of an attached `.test({ .. })` block, if any. A statement
        /// block (asserts plus whatever setup) that the `check` command runs as
        /// a test for this function; ignored by `run`/`build`. The `assert(c)`
        /// calls inside it are rewritten at parse time to `__assert(c, "loc")`.
        pub test_body: Option<Expr>,
        /// Functions declared inside the `.test({ .. })` block — helpers
        /// private to the test, never `pub`, unreachable from anything else.
        /// Filled by the parser and emptied by `hoist_test_fns.aipl` (run by
        /// `parse_and_partial_load`), which hoists each one to a top-level item
        /// and renames its uses in the test body and the helpers themselves; so
        /// every later pass sees only ordinary private functions, and this is
        /// always empty past the parser.
        pub test_fns: Vec<Function>,
        /// The text of the `# ..` doc comment above the declaration, if any —
        /// structured
        /// documentation for the function, surfaced by the `doc` command and
        /// ignored by `run`/`build`/`check`. A `"""..."""` raw string is
        /// de-dented like any other (the parser's raw-string hook runs first).
        pub doc: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct StructDecl {
        pub name: SpanStr,
        /// The `# ..` doc comment above the declaration, lines joined with
        /// newlines. `None` when it carries none.
        pub doc: Option<String>,
        /// Declared generic type parameters, e.g. `struct Box<T> { .. }` →
        /// `[TypeParam { name: "T", bound: Any }]`. Empty for an ordinary
        /// (non-generic) struct. A struct with type parameters is a *template*:
        /// it has no runtime layout of its own; each concrete use (`Box<i64>`,
        /// or an inferred `Box { value: 5 }` construction) is monomorphized into
        /// a synthetic named struct with these variables substituted.
        pub type_vars: Vec<TypeParam>,
        pub fields: Vec<FieldDecl>,
    }

    impl StructDecl {
        /// A struct is generic if it declares type parameters — i.e. it's a
        /// template that must be monomorphized before codegen sees it.
        pub fn is_generic(&self) -> bool {
            !self.type_vars.is_empty()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FieldDecl {
        pub name: SpanStr,
        pub ty: Type,
        pub default: Option<Expr>,
    }

    /// `variant Shape = Circle(i64) | Rect(i64, i64) | Empty;` — a tagged sum
    /// type (Haskell-style `data`, paren'd payloads). Each case carries zero or
    /// more positional payload types. Represented at runtime as an inline
    /// `{ tag: i64, payload }` composite sized to the widest case (like a tagged
    /// struct), addressed by pointer.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct VariantDecl {
        pub name: SpanStr,
        /// The `# ..` doc comment above the declaration, lines joined with
        /// newlines. `None` when it carries none.
        pub doc: Option<String>,
        /// Declared generic type parameters, e.g. `variant Opt<T> = Some(T) |
        /// Nothing` → `[TypeParam { name: "T", bound: Any }]`. Empty for an
        /// ordinary variant. Like [`StructDecl::type_vars`], a variant with type
        /// parameters is a template monomorphized per concrete instantiation.
        pub type_vars: Vec<TypeParam>,
        pub cases: Vec<VariantCase>,
    }

    impl VariantDecl {
        /// A variant is generic if it declares type parameters.
        pub fn is_generic(&self) -> bool {
            !self.type_vars.is_empty()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct VariantCase {
        pub name: SpanStr,
        /// The `# ..` doc comment above the declaration, lines joined with
        /// newlines. `None` when it carries none.
        pub doc: Option<String>,
        /// The payload slots, in declaration order; empty for a nullary case
        /// (e.g. `Empty`).
        pub payload: Vec<CaseParam>,
    }

    impl VariantCase {
        /// The payload's types alone, in declaration order — what every pass
        /// that doesn't care which slots were named needs (layout, patterns,
        /// codegen, arity checks).
        pub fn payload_tys(&self) -> Vec<Type> {
            self.payload.iter().map(|p| p.ty.clone()).collect()
        }
    }

    /// One slot of a variant case's payload. A case may spell a slot as a bare
    /// type (`Circle(i64)`), as a named one (`Circle(r: i64)`), or as a named
    /// one with a default (`Circle(r: i64 = 1)`) — the same three shapes a
    /// function parameter has, with the same meaning: a default is what makes
    /// the slot a *keyword* argument of the constructor.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CaseParam {
        /// The slot's declared name, or `None` for the bare positional form.
        /// An unnamed slot can only be passed positionally and cannot carry a
        /// default (there would be nothing to name it by at the call site).
        pub name: Option<String>,
        pub ty: Type,
        /// `Some(expr)` for a keyword slot, written `k: T = expr`. Exactly
        /// [`Param::default`]'s semantics, one construction at a time: keyword
        /// slots come after every positional one, may only be supplied by
        /// keyword (`Circle(1, color = "red")`), and a default may read the
        /// slots declared before it. The loader fills every omitted one at the
        /// construction site, so nothing downstream sees a partial payload.
        pub default: Option<Expr>,
        /// `true` for a slot written `k?: T = none` — see
        /// [`Param::implicit_some`], whose semantics this is one construction at
        /// a time. `Many(x, max?: u64 = none)` is constructed `Many(r, max = 2)`
        /// rather than `Many(r, max = some(2))`.
        pub implicit_some: bool,
        /// The `# ..` doc comment above the slot, lines joined with newlines —
        /// written inside the payload's parentheses, one slot per line. `None`
        /// when it carries none.
        pub doc: Option<String>,
    }

    /// How many arguments a parameter takes at a call site — see
    /// [`Param::arity`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Arity {
        /// An ordinary parameter: one argument of its type.
        One,
        /// `T*`: zero or more elements.
        ZeroOrMore,
        /// `T+`: one or more elements.
        OneOrMore,
    }

    impl Arity {
        /// Whether the parameter takes a sequence of arguments at all.
        pub fn is_variadic(self) -> bool {
            !matches!(self, Arity::One)
        }

        /// The marker written after the type: nothing, `*` or `+`.
        pub fn marker(self) -> &'static str {
            match self {
                Arity::One => "",
                Arity::ZeroOrMore => "*",
                Arity::OneOrMore => "+",
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Param {
        pub name: String,
        pub ty: Type,
        /// `true` for a `mut self` receiver: the function mutates this
        /// parameter (only valid on the first parameter, named `self`). Such a
        /// function returns nothing and is called as `v.f(...)`.
        pub mutable: bool,
        /// How many arguments the parameter takes: exactly one, or — for a
        /// variadic written `T*` (zero or more) or `T+` (one or more) — a
        /// sequence of them. For a variadic the stored `ty` is the *sequence
        /// type* the body sees — `str` when the element `T` is `char`,
        /// otherwise `T[]`, refined to `T[] without []` for `T+` — and the
        /// element type is recoverable from it (`str` → `char`, `Array(e)` →
        /// `e`). At a call site a `T*` also accepts a single element `T`
        /// (wrapped to a one-item sequence) or an optional `T?` (empty/one-item
        /// sequence); a `T+` accepts a single element or a `T[] without []`,
        /// the two shapes that cannot be empty. The normalization to the
        /// sequence type happens in codegen. The body is unaffected — it just
        /// sees a plain `ty`.
        pub arity: Arity,
        /// `Some(expr)` for a *keyword* parameter, written `k: T = expr`: having
        /// a default is exactly what makes a parameter a keyword parameter.
        /// Keyword parameters must come after every positional parameter, may
        /// only be supplied by keyword at a call site (`f(1, k = 2)`), and are
        /// not part of the function's type. The loader expands every call so
        /// each omitted keyword argument is filled from this default; after
        /// loading, the default is inert (calls are fully positional).
        ///
        /// The expression may read the parameters declared *before* it
        /// (`fn f(a: T, b: T = a)`) — and only those, since the fill happens at
        /// the call site, where the later parameters aren't decided yet. The
        /// loader binds each name the default reads to the argument that call
        /// passes for it; see `aipl-loader`'s `kwargs` module.
        pub default: Option<Expr>,
        /// `true` for a keyword parameter written `k?: T = none` — the `?` on
        /// the *name* rather than the type. [`ty`](Param::ty) is still the
        /// `T?` the body sees and [`default`](Param::default) is still `none`;
        /// what this changes is the call site, which passes a bare `T`
        /// (`max = 2`, not `max = some(2)`) and may not pass `none` at all.
        ///
        /// The two spellings describe the same parameter from opposite ends.
        /// `k: T? = none` says what the *body* gets and makes every caller
        /// restate it; `k?: T = none` says what a *caller* supplies and lets the
        /// optional stay an implementation detail of "you may leave this out".
        /// For an argument that is either given or absent — a bound, a limit —
        /// the second is what the call site means, and `some(..)` at every site
        /// is noise.
        ///
        /// The one shape a bare `T` cannot express is a wrapper passing on its
        /// *own* optional. `k?= expr` is the escape: it supplies the `T?`
        /// itself, unwrapped, and is accepted only against a parameter declared
        /// this way (see [`ExprKind::KwArg`]). Omitting the argument and
        /// forwarding a `none` are then the same thing, which is what lets a
        /// wrapper pass its parameter through faithfully.
        ///
        /// The loader's `kwargs` module is the only pass that reads this: it
        /// wraps each supplied argument in `some(..)` while expanding the call —
        /// all but a forwarded one, which is already an optional — so nothing
        /// downstream can tell the two spellings apart.
        pub implicit_some: bool,
    }

    /// The language's built-in scalar primitive types: the fixed-width integers
    /// (`i8`..`i64`, `u8`..`u64`), `bool`, `char`, and `str`. This is a *closed*
    /// set, so it's a proper enum rather than a stringly-typed name —
    /// `Type::Primitive(..)` is what used to be `Type::Named("i64")` and the
    /// like. (User structs, variants, generic type parameters, and the builtin
    /// `Error` type remain `Type::Named(String)`; the compiler's pseudo-type
    /// sentinels — `Any`, `NoneInner`, etc. — have their own `Type` variants.)
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Primitive {
        I8,
        I16,
        I32,
        I64,
        U8,
        U16,
        U32,
        U64,
        Bool,
        Char,
        Str,
    }

    impl Primitive {
        /// The primitive spelled `name`, if any (`"i64"` → `I64`). Lets the
        /// parser recognize a base-type identifier as a primitive vs a
        /// user/generic name.
        pub fn from_name(name: &str) -> Option<Primitive> {
            Some(match name {
                "i8" => Primitive::I8,
                "i16" => Primitive::I16,
                "i32" => Primitive::I32,
                "i64" => Primitive::I64,
                "u8" => Primitive::U8,
                "u16" => Primitive::U16,
                "u32" => Primitive::U32,
                "u64" => Primitive::U64,
                "bool" => Primitive::Bool,
                "char" => Primitive::Char,
                "str" => Primitive::Str,
                _ => return None,
            })
        }

        /// How the primitive is spelled in source (and rendered in diagnostics).
        pub fn name(self) -> &'static str {
            match self {
                Primitive::I8 => "i8",
                Primitive::I16 => "i16",
                Primitive::I32 => "i32",
                Primitive::I64 => "i64",
                Primitive::U8 => "u8",
                Primitive::U16 => "u16",
                Primitive::U32 => "u32",
                Primitive::U64 => "u64",
                Primitive::Bool => "bool",
                Primitive::Char => "char",
                Primitive::Str => "str",
            }
        }

        /// Whether this is one of the fixed-width integer types (`i8`..`u64`) —
        /// i.e. not `bool`/`char`/`str`.
        pub fn is_int(self) -> bool {
            self.int_bits().is_some()
        }

        /// Bit width if this is an integer type, else `None` (`bool`/`char`/`str`).
        pub fn int_bits(self) -> Option<u32> {
            Some(match self {
                Primitive::I8 | Primitive::U8 => 8,
                Primitive::I16 | Primitive::U16 => 16,
                Primitive::I32 | Primitive::U32 => 32,
                Primitive::I64 | Primitive::U64 => 64,
                _ => return None,
            })
        }

        /// Whether an integer type is signed (`i*`). `false` for the unsigned
        /// integers and for the non-integer primitives.
        pub fn int_signed(self) -> bool {
            matches!(
                self,
                Primitive::I8 | Primitive::I16 | Primitive::I32 | Primitive::I64
            )
        }

        /// Whether every value of integer type `self` is also a value of
        /// integer type `to`, so a value can change from one to the other
        /// without losing anything: a narrower type of the same signedness
        /// (`u8` → `u32`), or an unsigned type into a strictly wider signed one
        /// (`u32` → `i64`). A signed type never widens to an unsigned one,
        /// whose range has no negatives. `false` when either is not an
        /// integer, and for a type and itself.
        ///
        /// Free at runtime: an integer is held canonically in an `i64`
        /// register (sign-extended for `i*`, zero-extended for `u*`), and a
        /// value both types can represent has the same canonical bits in each.
        pub fn widens_to(self, to: Primitive) -> bool {
            let (Some(from_bits), Some(to_bits)) = (self.int_bits(), to.int_bits()) else {
                return false;
            };
            from_bits < to_bits && !(self.int_signed() && !to.int_signed())
        }
    }

    pub fn is_unit(t: &Type) -> bool {
        matches!(t, Type::Unit)
    }

    /// What a walk over a set promises — see [`Type::Set`]. Carried on the
    /// type, so `#{T}`, `#>{T}` and `#<{T}` are three types that share a runtime.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum SetOrder {
        /// `#{T}`: no promised order.
        Unordered,
        /// `#>{T}`: largest element first.
        Desc,
        /// `#<{T}`: smallest element first.
        Asc,
        /// Decided by the use site — the order of `xs.to_set()` before the
        /// expected type has said which set it builds, the way a bare `none`
        /// or `[]` is typed by a placeholder until context fills it in. A
        /// checker placeholder only: it coerces to any order, is never
        /// recorded as an answer, and codegen resolves it (to the locked
        /// order, else `Unordered`) at the one call that produces it. No
        /// value ever has it.
        Context,
    }

    impl SetOrder {
        /// The character between the `#` and the `{` that spells this order —
        /// nothing for the unordered set, and for the placeholder, which reads
        /// as the plain set it becomes without context.
        pub fn spelling(self) -> &'static str {
            match self {
                SetOrder::Unordered | SetOrder::Context => "",
                SetOrder::Desc => ">",
                SetOrder::Asc => "<",
            }
        }

        /// Whether the set keeps its elements sorted at all.
        pub fn is_ordered(self) -> bool {
            !matches!(self, SetOrder::Unordered)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Type {
        /// The unit type — what a function with no declared return type produces:
        /// nothing. It never appears as a parameter, field, array element, or
        /// optional inner — the grammar can't express it there — so type validation
        /// treats it as an unknown scalar and rejects it in those positions, leaving
        /// the function return as its only home.
        Unit,
        /// A built-in scalar primitive (`i64`, `bool`, `str`, …). See
        /// [`Primitive`].
        Primitive(Primitive),
        /// A name that isn't a primitive: a user struct or variant, or the
        /// builtin `Error` type. (Compiler pseudo-type sentinels that used to
        /// overload this — `__none__`, `any`, etc. — have their own dedicated
        /// variants below instead, and so does a generic type parameter: see
        /// [`Type::TypeVar`].)
        Named(String),
        /// `Case<V>` — one *case* of the variant `V`, carrying no payload.
        ///
        /// A variant value is a tag plus a payload; a `Case<V>` is the tag
        /// alone. It exists because naming a case is not the same as having a
        /// value of one: `Term(Str)` in a grammar wants to say "any token whose
        /// kind is `Str`", and before this it had to construct one — `Term(Str(""))`
        /// — with a payload that was never read and had to be invented.
        ///
        /// There is deliberately no syntax to get a payload out of one, because
        /// there is no payload in it: the type is what makes the dummy value
        /// unnecessary rather than merely conventional. Its runtime form is the
        /// tag as a `u64` ([`ConcreteType::Case`]), so it costs one register and
        /// no allocation, and `==` on two of them is an integer compare.
        ///
        /// The inner type is the variant, and is always a [`Type::Named`] (or a
        /// [`Type::TypeVar`] standing for one) — `Case<i64>` is refused at type
        /// validation, where the "is this a variant" question can be answered.
        Case(Box<Type>),
        /// A generic type parameter — the `T` of `fn f<T: any>(x: T)`, and the
        /// synthetic variable an anonymous `any` normalizes to (which carries an
        /// empty name).
        ///
        /// Its own variant rather than a [`Type::Named`] holding the variable's
        /// name, because the two are not interchangeable and telling them apart
        /// by string was the source of real bugs: a substitution keyed by name
        /// rewrote a *struct* that happened to share a type parameter's name,
        /// and "does this type mention `T`" could not distinguish the parameter
        /// from a type called `T`. The passes also disagreed about the encoding
        /// — the checker used a `__typevar__$T` sentinel, monomorphization the
        /// bare name — so a type crossing between them meant two different
        /// things.
        ///
        /// Anything holding one of these is *abstract*: it is not a runtime type
        /// and codegen never sees it, because monomorphization substitutes every
        /// one away before then.
        TypeVar(String),
        /// `T?` — optional T. Represented at runtime as a 16-byte
        /// stack value `{ tag: i64, value: i64 }` (tag 0 = None,
        /// 1 = Some), passed by pointer like a struct.
        Optional(Box<Type>),
        /// `T[]` — a growable array of `T` (T ∈ i64/bool/char). A
        /// refcounted heap block laid out as `[refcount: i64][len: i64]
        /// [elem0: i64]...`; the pointer the language holds points at the
        /// `len` field (so `ptr - 8` is the refcount, matching strings).
        Array(Box<Type>),
        /// `#{T}` — a set of `T` (T ∈ i64/bool/char/str).
        /// Represented at runtime exactly like an `Array(T)` — the same
        /// refcounted heap block — but constructed deduplicated and given a
        /// distinct type so it isn't index-able or array-assignable, renders
        /// as `{a, b, c}`, and offers `contains`/`len`.
        ///
        /// The [`SetOrder`] says what a walk over it promises: nothing for
        /// `#{T}`, largest first for `#>{T}`, smallest first for `#<{T}`. The
        /// ordered forms keep the block sorted and require an `ord` element;
        /// all three are distinct types.
        Set(Box<Type>, SetOrder),
        /// `#{K: V}` — a dictionary mapping keys of type `K` (a scalar/`str`,
        /// like a set element) to values of type `V` (any value type). Stored
        /// at runtime as a refcounted heap block holding an array of
        /// `(key, value)` pairs (each pair laid out as the 8-byte key followed
        /// by the value); the language holds a single pointer to it, like an
        /// array/set. Renders as `{k: v, ...}`, offers `get`/`contains_key`/`len`.
        Dict(Box<Type>, Box<Type>),
        /// `T!E` — a result: either `ok(T)` or `err(E)`. Represented like a
        /// 2-case variant / a non-nested optional: a 16-byte inline value
        /// `{ tag: i64, value }` (tag 1 = Ok, 0 = Err; the 8-byte `value` holds
        /// the Ok or Err payload), addressed by pointer. v1 payloads are
        /// scalar/`str` (8 bytes each). Inspected with `match (r) { ok(v) => ..,
        /// err(e) => .. }`, propagated with the postfix `?` operator.
        Result(Box<Type>, Box<Type>),
        /// `(A, B) -> R` — the type of a lambda parameter. This is a
        /// *compile-time only* type: lambdas are monomorphized away (the
        /// receiving function is specialized per lambda), so there is no
        /// runtime function value. Valid only as a parameter type; a value of
        /// this type can be called or passed on, never stored or returned.
        Fn(Vec<Type>, Box<Type>),
        /// `(A, B, C)` — a tuple of 2+ types, stored inline like a struct,
        /// addressed by pointer (sret). Lowered to a synthetic named struct
        /// `__tuple$A$B$C` before type-checking, so only the parser and the
        /// `lower_tuples` pre-pass ever see this variant.
        Tuple(Vec<Type>),
        /// `Foo<A, B>` — a use of a generic struct/variant with concrete type
        /// arguments. Lowered to a synthetic monomorphic named type
        /// (`Foo$A$B`) by mono's `lower_generics` pre-pass — substituting the
        /// template's declared type variables with these arguments — before
        /// type-checking, so only the parser and that pre-pass ever see this
        /// variant. The `String` is the template's base name; the `Vec` its
        /// concrete type arguments (never empty).
        Generic(String, Vec<Type>),
        /// The anonymous generic bound keyword `any`, as written in `any[]`/
        /// `any?` — parsed directly from source. Monomorphization's `normalize`
        /// replaces each occurrence with a synthetic named type variable before
        /// anything else sees it.
        Any,
        /// The placeholder element/inner of an untyped `none`, empty array
        /// literal (`[]`), or the empty brace literal (`#{}`) — coerces to
        /// any element/inner type at the use site (see [`crate::is_none_inner`]).
        NoneInner,
        /// Monomorphization-only: the pseudo-type a generic's type variable is
        /// bound to when the only argument that could pin it is an empty array
        /// literal (see the fallback pass in `instantiate_types`). Substituted
        /// back to `Array(NoneInner)` once it lands in a container, so existing
        /// codegen treats it as an ordinary empty array.
        EmptyArrayArg,
        /// Monomorphization-only: like `EmptyArrayArg`, but for a bare `none`
        /// literal — substituted back to `Optional(NoneInner)`.
        NoneLiteralArg,
        /// Checker-only: a type it cannot pin down — chiefly the result of a
        /// generic call whose type variables this pass does not instantiate, and
        /// what `subst_vars` yields for a variable nothing bound. It is a
        /// *wildcard*: it coerces with anything in either direction, so the
        /// checker stays permissive rather than reporting a mismatch it cannot
        /// justify, and it renders as `_`.
        ///
        /// Deliberately not a [`Type::Named`] sentinel, which is what it was
        /// (`__unknown__`) — for the reason given there and at
        /// [`Type::TypeVar`]: a pseudo-type spelled as a name is one string
        /// comparison away from a user type that happens to share the spelling,
        /// and every pass that must not treat it as a struct has to remember to
        /// ask. As a variant, `match` asks for it.
        ///
        /// Abstract, like [`Type::TypeVar`]: monomorphization resolves or
        /// rejects it, so [`Type::to_concrete`] refuses it and codegen never
        /// sees one.
        Unknown,
        /// A `str` produced by `+`-concatenating two strings — distinguished
        /// from a plain `str` so codegen can specialize a lazy-concat
        /// representation for it (see [`crate::is_concat_str`]). Only meaningful as
        /// the type of a scalar value flowing to a `str` parameter; decays to
        /// a plain `str` once it's placed into any other container/context.
        ConcatStr,
        /// `T without X` — the base type `T` *refined*: every value of `T`
        /// except the one the [`Exclusion`] names. `i64[] without []` is a
        /// non-empty array of `i64`.
        ///
        /// A refinement is a type-system fact only. At runtime a
        /// `T[] without []` is exactly a `T[]` — same block, same layout — so a
        /// refined value is usable wherever its base is wanted (the checker's
        /// `coerce` peels the refinement), while the reverse needs proof: a
        /// literal the checker can see is not the excluded value, or a builtin
        /// that tests for it at runtime (`ensure_nonempty`). Nothing past the
        /// checker sees the variant: [`crate::erase_refinements`] rewrites every
        /// refined type to its base before monomorphization, which is what
        /// keeps the representation-level passes honest about there being no
        /// second representation.
        Without(Box<Type>, Exclusion),
    }

    /// What a `T without X` type leaves out — the `X`. One case per kind of
    /// value the checker knows how to exclude: the empty array, or a case of a
    /// variant.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub enum Exclusion {
        /// `[]` — the empty array.
        EmptyArray,
        /// A case of the variant the base names: `V without C`. The bare case
        /// name, as it is declared — cases are never mangled, so the name means
        /// the same wherever the type is read.
        NamedCase(String),
    }

    impl Exclusion {
        /// The excluded value as the user writes it after `without`.
        pub fn spelling(&self) -> String {
            match self {
                Exclusion::EmptyArray => "[]".to_string(),
                Exclusion::NamedCase(name) => name.clone(),
            }
        }
    }

    /// A type with every abstraction resolved — what exists *after*
    /// monomorphization, and the only thing codegen ever sees.
    ///
    /// The same shape as [`Type`] minus the four variants that stand for
    /// something not yet decided: [`Type::TypeVar`] and [`Type::Any`] (a
    /// parameter monomorphization substitutes away), and [`Type::Tuple`] and
    /// [`Type::Generic`] (surface syntax lowered to a synthetic named type
    /// before type-checking). Dropping them is the point: an algorithm that
    /// only runs post-mono can match exhaustively without an arm whose body is
    /// `unreachable!()`, and the compiler enforces that it never receives one
    /// rather than the author remembering to panic.
    ///
    /// The context-decided placeholders — `NoneInner`, `EmptyArrayArg`,
    /// `NoneLiteralArg` — *are* here, because they genuinely reach codegen: an
    /// empty `[]` still has an element type of some kind when its drop function
    /// is picked. They are a different axis from abstractness (undecided rather
    /// than universally quantified) and want their own treatment eventually.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub enum ConcreteType {
        Unit,
        Primitive(Primitive),
        Named(String),
        Optional(Box<ConcreteType>),
        Array(Box<ConcreteType>),
        Set(Box<ConcreteType>, SetOrder),
        Dict(Box<ConcreteType>, Box<ConcreteType>),
        Result(Box<ConcreteType>, Box<ConcreteType>),
        Fn(Vec<ConcreteType>, Box<ConcreteType>),
        /// `Case<V>` with `V` resolved — see [`Type::Case`]. Holds the variant's
        /// concrete name rather than its type, because the tag is all there is
        /// at runtime and the name is only needed to render a case back
        /// (`case_name`) and to keep two variants' cases from comparing equal.
        Case(String),
        NoneInner,
        EmptyArrayArg,
        NoneLiteralArg,
        ConcatStr,
    }

    impl Type {
        /// This type with every abstraction resolved, or `None` if it still
        /// mentions one — a type variable, a bare `any`, or a tuple/generic
        /// application that should have been lowered.
        ///
        /// The failure is the useful half: monomorphization converts here, so a
        /// variable that escaped instantiation is caught at the boundary with a
        /// name to report, instead of reaching codegen and being mangled into a
        /// nonsense instance.
        pub fn to_concrete(&self) -> Option<ConcreteType> {
            Some(match self {
                Type::TypeVar(_)
                | Type::Any
                | Type::Unknown
                | Type::Tuple(_)
                | Type::Generic(..) => return None,
                Type::Unit => ConcreteType::Unit,
                Type::Primitive(p) => ConcreteType::Primitive(*p),
                Type::Named(n) => ConcreteType::Named(n.clone()),
                Type::NoneInner => ConcreteType::NoneInner,
                Type::EmptyArrayArg => ConcreteType::EmptyArrayArg,
                Type::NoneLiteralArg => ConcreteType::NoneLiteralArg,
                Type::ConcatStr => ConcreteType::ConcatStr,
                // A refinement has its base's representation, and by the time
                // anything asks for a concrete type it has been erased anyway
                // (see `erase_refinements`).
                Type::Without(base, _) => base.to_concrete()?,
                Type::Optional(i) => ConcreteType::Optional(Box::new(i.to_concrete()?)),
                Type::Array(i) => ConcreteType::Array(Box::new(i.to_concrete()?)),
                Type::Set(i, o) => ConcreteType::Set(Box::new(i.to_concrete()?), *o),
                Type::Dict(k, v) => {
                    ConcreteType::Dict(Box::new(k.to_concrete()?), Box::new(v.to_concrete()?))
                }
                Type::Result(a, b) => {
                    ConcreteType::Result(Box::new(a.to_concrete()?), Box::new(b.to_concrete()?))
                }
                Type::Fn(ps, r) => ConcreteType::Fn(
                    ps.iter()
                        .map(Type::to_concrete)
                        .collect::<Option<Vec<_>>>()?,
                    Box::new(r.to_concrete()?),
                ),
                // The variant has to have resolved to a name by now; a `Case`
                // still over a type variable is as abstract as the variable is.
                Type::Case(v) => match v.to_concrete()? {
                    ConcreteType::Named(n) => ConcreteType::Case(n),
                    _ => return None,
                },
            })
        }
    }

    impl ConcreteType {
        /// This type widened back to the abstract representation, for the
        /// pre-mono machinery that still speaks [`Type`]. Always succeeds —
        /// every concrete variant has an abstract counterpart.
        pub fn widen(&self) -> Type {
            match self {
                ConcreteType::Unit => Type::Unit,
                ConcreteType::Primitive(p) => Type::Primitive(*p),
                ConcreteType::Named(n) => Type::Named(n.clone()),
                ConcreteType::Case(n) => Type::Case(Box::new(Type::Named(n.clone()))),
                ConcreteType::NoneInner => Type::NoneInner,
                ConcreteType::EmptyArrayArg => Type::EmptyArrayArg,
                ConcreteType::NoneLiteralArg => Type::NoneLiteralArg,
                ConcreteType::ConcatStr => Type::ConcatStr,
                ConcreteType::Optional(i) => Type::Optional(Box::new(i.widen())),
                ConcreteType::Array(i) => Type::Array(Box::new(i.widen())),
                ConcreteType::Set(i, o) => Type::Set(Box::new(i.widen()), *o),
                ConcreteType::Dict(k, v) => Type::Dict(Box::new(k.widen()), Box::new(v.widen())),
                ConcreteType::Result(a, b) => {
                    Type::Result(Box::new(a.widen()), Box::new(b.widen()))
                }
                ConcreteType::Fn(ps, r) => Type::Fn(
                    ps.iter().map(ConcreteType::widen).collect(),
                    Box::new(r.widen()),
                ),
            }
        }
    }

    /// A struct declaration monomorphization has finished with: no type
    /// parameters (a template is instantiated per use, so what survives is
    /// always an instance) and no field defaults (already expanded into the
    /// construction sites that omitted them).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConcreteStructDecl {
        pub name: String,
        pub fields: Vec<ConcreteFieldDecl>,
        /// Whether this struct is what a tuple lowered to. Its name and field
        /// names are then the compiler's, not the user's — so a renderer prints
        /// the value as it was written, `(1, "a")`, and never the struct.
        pub is_tuple: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConcreteFieldDecl {
        pub name: String,
        pub ty: ConcreteType,
    }

    /// A variant declaration monomorphization has finished with — the
    /// [`ConcreteStructDecl`] counterpart for sum types.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConcreteVariantDecl {
        pub name: String,
        pub cases: Vec<ConcreteVariantCase>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConcreteVariantCase {
        pub name: String,
        pub payload: Vec<ConcreteType>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FieldInit {
        /// The field's name and where it is written in `T { name: v }` or the
        /// shorthand `T { name }`; empty for a spread, and spanless for a
        /// synthesized init.
        pub name: SpanStr,
        pub value: Expr,
    }

    /// An expression with its source span. Equality ignores the span so
    /// pre-span tests still work.
    #[derive(Debug, Clone)]
    pub struct Expr {
        pub kind: ExprKind,
        pub span: Span,
        /// The type the checker resolved for this expression, when knowing it
        /// later matters more than re-deriving it.
        ///
        /// Only *context-dependent* expressions carry one: a bare `none`, an
        /// empty `[]`/`#{}`, an `ok`/`err` whose other side is a
        /// placeholder, and a generic constructor its own arguments don't pin.
        /// Their types come from where they sit, so any pass that *moves* them —
        /// inlining, folding — would otherwise change what they mean. Recording
        /// the type at the point the context is still visible makes the move
        /// safe, and is why `check` returns a rewritten program rather than only
        /// errors.
        ///
        /// `None` everywhere else: the type is recoverable from the expression
        /// itself, and duplicating it would just be a second thing to keep true.
        /// Boxed: `None` on the overwhelming majority of nodes, and `Expr` is
        /// recursed over deeply enough during compilation that widening every
        /// one of them by a whole `Type` costs real stack.
        pub ty: Option<Box<Type>>,
        /// The span of the sub-expression that produces this one's *value*: for
        /// a block, its trailing expression rather than the whole block.
        ///
        /// A block's own `span` is deliberately the whole block — that is what
        /// an error *about the block* should underline. But a diagnostic about
        /// the block's **value**, chiefly a return-type mismatch, wants the
        /// expression that produced that value; reporting the block span
        /// instead pointed at whichever statement happened to come first.
        ///
        /// Recorded by the parser, once, at the point it builds the block and
        /// has the tail in hand — rather than re-derived by walking the
        /// `Seq`/`Let`/`LetMut`/`Assign` wrapper chain at each diagnostic.
        ///
        /// `None` on everything else, where the value *is* the expression and
        /// `span` already answers the question. Boxed for the same reason as
        /// [`Expr::ty`]: `None` on the overwhelming majority of nodes, and
        /// `Expr` is recursed over deeply enough during compilation that
        /// widening every one of them costs real stack.
        pub value_span: Option<Box<Span>>,
        /// Where this node's *name* is written: the field in `p.x`, the callee
        /// in `f(a)` and `p.f(a)`, the identifier of an `Ident` or a
        /// `Construct`, a binder. `span` covers the whole construct and cannot
        /// say where inside it the name sits, which is what a language server
        /// needs to map a cursor to a declaration.
        ///
        /// `None` on a synthesized node, which has no name written anywhere.
        /// Boxed for the same reason as [`Expr::ty`].
        pub name_span: Option<Box<Span>>,
    }

    impl Expr {
        pub fn new(kind: ExprKind, span: Span) -> Self {
            Self {
                kind,
                span,
                ty: None,
                value_span: None,
                name_span: None,
            }
        }

        /// The same expression with the span of its name recorded — see
        /// [`Expr::name_span`].
        pub fn with_name_span(mut self, span: Span) -> Self {
            self.name_span = Some(Box::new(span));
            self
        }

        /// The span to report a diagnostic about this expression's *value*
        /// against — the recorded [`Expr::value_span`] when the parser stored
        /// one, otherwise the expression's own span.
        pub fn value_span(&self) -> &Span {
            self.value_span.as_deref().unwrap_or(&self.span)
        }

        /// The same expression with the span of its value recorded — see
        /// [`Expr::value_span`].
        pub fn with_value_span(mut self, span: Span) -> Self {
            self.value_span = Some(Box::new(span));
            self
        }

        /// `kind` in place of `like`'s, keeping its span *and* its recorded type.
        ///
        /// The idiom for a pass that rewrites an expression's children: building
        /// with [`Expr::new`] instead silently drops [`Expr::ty`], which is
        /// exactly the information the rewriting pass is liable to invalidate by
        /// moving things around.
        pub fn rebuilt(kind: ExprKind, like: &Expr) -> Self {
            Self {
                kind,
                span: like.span.clone(),
                ty: like.ty.clone(),
                value_span: like.value_span.clone(),
                name_span: like.name_span.clone(),
            }
        }

        /// The same expression with its resolved type recorded — see [`Expr::ty`].
        pub fn with_ty(mut self, ty: Type) -> Self {
            self.ty = Some(Box::new(ty));
            self
        }
    }

    impl PartialEq for Expr {
        /// Ignores `span` *and* `ty`: both are derived from where an expression
        /// sits rather than what it says, and tests compare shapes.
        fn eq(&self, other: &Self) -> bool {
            self.kind == other.kind
        }
    }

    impl Eq for Expr {}

    /// Decompose an assignment LHS (a place expression) into its base binding
    /// name and the field path leading from it, outermost field last —
    /// `a.b.c` yields `("a", ["b", "c"])`, a bare `a` yields `("a", [])`.
    /// `None` for any other expression shape (not a valid place).
    pub fn assign_target(lhs: &Expr) -> Option<(&str, Vec<&str>)> {
        let mut path = Vec::new();
        let mut cur = lhs;
        loop {
            match &cur.kind {
                ExprKind::Ident(name) => {
                    path.reverse();
                    return Some((name, path));
                }
                ExprKind::Field(obj, field) => {
                    path.push(field.as_str());
                    cur = obj;
                }
                _ => return None,
            }
        }
    }

    /// The source spelling of an assignment LHS prefix — the base name plus
    /// the first `depth` fields, dotted (`"a.b"`). For diagnostics.
    pub fn assign_target_display(name: &str, path: &[&str], depth: usize) -> String {
        let mut s = name.to_string();
        for f in &path[..depth] {
            s.push('.');
            s.push_str(f);
        }
        s
    }

    /// Which *operation* an operator performs. **Not an AST node:** an operator
    /// use is an [`ExprKind::Call`] named for the operator's spelling, which the
    /// loader resolves to a canonical `__builtin_*` impl like any other imported
    /// name. This enum is the opcode behind those names, reached through
    /// [`binop_for_builtin`] — plus the parser's precedence table, the one place
    /// a spelling still needs an operation before resolution has happened.
    ///
    /// Variants name the **builtin** an operator resolves to, never the spelling
    /// a source file imported it as — `Concat`, not `+++`. Which alias a file
    /// writes is a property of its import list (`OPERATOR_BUILTINS`); after
    /// import resolution nothing downstream can tell, because there is no
    /// operator left to tell about.
    ///
    /// Where an operator has more than one semantics (`wrapping_add` vs
    /// `saturating_add`) the flavor lives in the *canonical name*, not here —
    /// which is why [`binop_for_builtin`] deliberately does not answer for the
    /// arithmetic impls: one opcode cannot distinguish them.
    ///
    /// This was a bare `char` with a private encoding (`'E'` for `==`, `'C'` for
    /// `+++`), which two passes independently got wrong by testing `'+'` — the
    /// opcode for addition — when they meant concatenation. Nothing caught it:
    /// every `char` is a valid pattern, so a wrong one is a match arm that
    /// silently never fires. The enum makes those matches exhaustive instead.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum BinOp {
        /// `wrapping_add` / `saturating_add`.
        Add,
        /// `wrapping_sub` / `saturating_sub`.
        Sub,
        /// `wrapping_mul`.
        Mul,
        /// `saturating_divide`.
        Div,
        /// `saturating_remainder`.
        Rem,
        /// `less_than`.
        Lt,
        /// `greater_than`.
        Gt,
        /// `less_than_or_equal`.
        Le,
        /// `greater_than_or_equal`.
        Ge,
        /// `equal`.
        Eq,
        /// `not_equal`.
        Ne,
        /// `logical_and`.
        And,
        /// `logical_or`.
        Or,
        /// `concat`.
        Concat,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ExprKind {
        Num(i64),
        Bool(bool),
        Str(String),
        /// Single ASCII byte. UTF-8 codepoints above 0x7F are rejected at
        /// lex time so the language stays byte-deterministic.
        Char(u8),
        Ident(String),
        /// A function or method call. `args` is the full effective argument
        /// list; when `method_style` (the trailing `bool`) is `true` the call
        /// was written `recv.f(a, b)` and the receiver is `args[0]` — i.e. it
        /// is stored exactly as the free call `f(recv, a, b)`. The flag is the
        /// one bit that distinguishes the two surface forms, and it is
        /// semantically load-bearing: only a `self`-function may be called
        /// method-style, and the in-place writeback form of a mutating method
        /// (`set recv.f(..)`) requires method syntax on a mutable variable.
        /// A mutating method called anywhere *else* — including the free-call
        /// form `f(recv, ..)` — is copy-and-modify: mono rewrites it to copy
        /// the receiver, mutate the copy, and yield it (see the `mutating`
        /// desugar in `aipl-mono`), so both `xs.push(x)` and `push(xs, x)` in
        /// expression position yield a fresh array and leave `xs` alone.
        /// Non-mutating calls are indifferent to the flag
        /// (`x.to_str()` ≡ `to_str(x)`).
        ///
        /// The callee is a [`Callee`]: an arm per function the compiler
        /// knows (builtins, operator impls, `some`/`ok`/`err`, the
        /// intrinsics passes synthesize) and [`Callee::User`] for a name from
        /// source. Before the loader resolves imports every source-spelled
        /// callee is a `User` — `map`, `+`, or whatever alias the file wrote.
        Call(Callee, Vec<Expr>, bool),
        Neg(Box<Expr>),
        If(Box<Expr>, Box<Expr>, Box<Expr>),
        Construct(String, Vec<FieldInit>),
        Field(Box<Expr>, String),
        /// `let name = value; body` — immutable binding. The `Option<Type>` is
        /// the optional annotation from `let name: T = value;`: when present it
        /// is the binding's declared type, so `value` is checked *against* it
        /// (and a bare integer literal flexes to it) instead of the binding
        /// simply taking whatever `value` inferred to. `None` is the
        /// unannotated form, and is what every synthesized binding uses.
        Let(String, Option<Type>, Box<Expr>, Box<Expr>),
        /// `mut name = value; body` — mutable binding, lives in a stack slot.
        /// The `Option<Type>` is the `mut name: T = value;` annotation, with
        /// the same meaning as [`ExprKind::Let`]'s. It matters more here: a
        /// `mut` binding's type is fixed at its declaration and every later
        /// `set` must match it, so the annotation is how a counter is pinned to
        /// a narrow width (`mut n: u8 = 0;`) with nothing else to infer from.
        LetMut(String, Option<Type>, Box<Expr>, Box<Expr>),
        /// `set lhs = value; body` — store to an existing mut binding. The
        /// LHS is a *place* expression: a bare `Ident` (store to the binding
        /// itself) or a `Field` chain of any depth rooted at one
        /// (`set a.b.c = v;`). A field-chain store is a functional update
        /// with value semantics: mono's `infer` desugars it to nested
        /// constructs (`set a = A { b: B { c: v, ... }, ... }`) once the
        /// struct types are known, so aliases of the old value are unaffected
        /// and every pass after mono only ever sees a bare-`Ident` LHS.
        Assign(Box<Expr>, Box<Expr>, Box<Expr>),
        /// `for (let var : iterable) { body }` — iterates each byte of
        /// `iterable` (a str) until NUL, binding `var: char` per iteration.
        /// Body's value is discarded; the loop expression itself is i64 0.
        ///
        /// The second field is the *index* binder of the two-binder form
        /// `for (let i, x : xs)`, or `None` for the plain `for (let x : xs)`.
        /// What an index is depends on what is being iterated, which is why it
        /// survives parsing instead of being desugared there: over an array or
        /// a `str` it is the `u64` position, and over a dict it is the key —
        /// a dict is indexed by its key exactly as an array is by its
        /// position. Mono is the first pass that knows which, and it is where
        /// the index is lowered (`lower_for`).
        For(String, Option<String>, Box<Expr>, Box<Expr>),
        /// `while (cond) { body }` — re-evaluates `cond` (a bool) before each
        /// iteration and runs `body` while it holds. Body's value is discarded;
        /// the loop expression itself is i64 0 (like `For`).
        While(Box<Expr>, Box<Expr>),
        /// `shim <effect> { op = f, .. } { body }` — install shim functions for
        /// an effect's operations over the *dynamic* extent of `body`: every
        /// call made while it runs, at any depth, reaches the bound function
        /// instead of the real one. Carries the effect name, the bindings as
        /// `(operation, function)` pairs in source order, and the body.
        ///
        /// Discharges the effect: `body` may call operations of `<effect>`
        /// without the enclosing function declaring it, since with every
        /// operation bound the body cannot reach the underlying resource. The
        /// bound functions' *own* effects still propagate to the enclosing
        /// function. See `SHIMMABLE_EFFECTS`.
        ///
        /// A binding may be written as a closure, `op = |args| body`. The parser
        /// gives it a name of its own ([`SHIM_CLOSURE_PREFIX`]) and wraps the shim
        /// in a `let` of that name whose value is the lambda, so this node still
        /// binds names only; `aipl-mono`'s `shim_closures` lowers the `let`.
        Shim(String, Vec<(String, String)>, Box<Expr>),
        /// `none` — the None value. Its type is determined by context
        /// (function return, function arg, or the other branch of an
        /// if/else), at which point it materializes as a stack slot
        /// with tag 0.
        None,
        /// `match (scrutinee) { ... }` — inspect an optional (`some(v)`/`none`),
        /// a result (`ok(v)`/`err(e)`), a variant (its case names), or a `str`
        /// (string-literal arms `"foo" => e` with a trailing `_ => e` default).
        /// A constructor arm's binding (e.g. `v`) is only in scope in that arm,
        /// where the tag has been checked. Exhaustiveness is enforced: a tagged
        /// match must cover every case, a `str` match must end with `_`. See
        /// [`MatchArm`].
        Match(Box<Expr>, Vec<MatchArm>),
        /// `if (let PATTERN = EXPR) { THEN } (else { ELSE })?` — bind
        /// `PATTERN`'s payload from `EXPR` and run `THEN` if it matches, `ELSE`
        /// otherwise (`ELSE` is a synthetic `Unit` when no `else` is written,
        /// exactly as an else-less `if` synthesizes one). The `MatchArm` pairs
        /// `PATTERN` with `THEN` as its body — the same shape a `match` arm for
        /// this one case would have — and its `span` is the pattern's own, so a
        /// binding-count/type error underlines the pattern rather than the whole
        /// construct.
        ///
        /// Unlike [`Match`], only this one case is named: `PATTERN` says which
        /// case this site cares about, and every other case — including, for a
        /// `str`/`char`/array scrutinee, every non-matching value — silently
        /// falls to `ELSE`. So unlike `match`, adding a case to a variant never
        /// forces an `if let` site to be revisited; that is the point of writing
        /// one instead of a `match` with an empty arm per other case.
        ///
        /// Parenthesized (`if (let ..) { .. }`) for the same reason every other
        /// control-flow condition in this grammar is (`if`, `while`, `match`,
        /// `for`): a bare `if let PAT = EXPR { .. }` cannot tell an
        /// unparenthesized struct construction (`IDENT { .. }`) ending `EXPR`
        /// from the `if`'s own opening brace.
        ///
        /// `PATTERN` is [`Pattern`]'s ordinary grammar (a constructor, nullary or
        /// variant-qualified, plus a string/char literal) — never an array
        /// pattern, which the parser only reaches through `match_arm`'s separate
        /// `[e0, e1, ...] => ..` alternative, not through `Pattern` itself.
        ///
        /// Eliminated before codegen: monomorphization's `infer` desugars it into
        /// a genuine, exhaustive [`Match`] once the scrutinee's concrete case
        /// list is known — one arm for `PATTERN`, plus one synthesized arm per
        /// remaining case (or, for a `str`/`char`/array scrutinee, a single
        /// [`Pattern::Wildcard`] fallthrough), each running a cloned, already-
        /// inferred copy of `ELSE`. Codegen never has to know `if let` from
        /// `match`.
        IfLet(Box<MatchArm>, Box<Expr>, Box<Expr>),
        /// `[e0, e1, ...]` — an array literal. Element types must all
        /// agree (and be a primitive). An empty `[]` has element type
        /// `__none__` and coerces to any `T[]`, like bare `none`.
        ///
        /// An element may be a [`ExprKind::Spread`], which splices an array's
        /// elements in rather than nesting it. Mono's `infer` rewrites any
        /// literal containing one into a seed-and-append block, so every pass
        /// after mono sees only plain elements.
        ArrayLit(Vec<Expr>),
        /// `..xs` inside an array literal — splice `xs`'s elements into the
        /// literal at this position. Legal *only* as a direct element of
        /// [`ExprKind::ArrayLit`]; the parser rejects it anywhere else (a call
        /// argument, an array pattern), so it never reaches the checker on its
        /// own. Mono desugars it away, so codegen never sees one.
        Spread(Box<Expr>),
        /// `#{e0, e1, ...}` — a set literal. Elements must share one type
        /// (i64/bool/char/str); duplicates are dropped at construction (by value
        /// for scalars, by content for `str`) so the value holds each distinct
        /// element once.
        ///
        /// An empty `#{}` is parsed as this, with element type `__none__`, but it
        /// is the empty literal for a **dict** as well: nothing in an empty brace
        /// pair says which, so it takes its type from the use site the way `[]`,
        /// `none` and a bare integer do (see `coerce`). `#{__none__}` cannot
        /// arise any other way — a non-empty set takes its element type from its
        /// first element — so that type *is* the marker.
        ///
        /// The [`SetOrder`] is the literal's own: `#>{..}` / `#<{..}` build an
        /// ordered set (and an empty one of those is a set, never a dict).
        SetLit(Vec<Expr>, SetOrder),
        /// `#{k0: v0, k1: v1, ...}` — a dict literal. Keys must share one
        /// scalar/`str` type and values one value type; duplicate keys keep the
        /// last binding (by value for scalars, by content for `str`).
        ///
        /// An empty dict is written `#{}`, which flexes to whichever of set or
        /// dict the use site wants (see [`ExprKind::SetLit`]) — so an empty
        /// `DictLit` never comes from the parser. With no context to decide from,
        /// as in an unannotated binding, `#{}` settles on a set; annotate to get
        /// a dict.
        DictLit(Vec<(Expr, Expr)>),
        /// `receiver[index]` — array indexing. Evaluates to `T?`: the
        /// element wrapped in `some` when in bounds, else `none`. An index
        /// whose type is the builtin `Span` struct means slicing instead —
        /// `recv[span]` is sugar for `recv[span.start..span.end]` (see
        /// [`Slice`](Self::Slice)); the passes dispatch on the index type.
        Index(Box<Expr>, Box<Expr>),
        /// `receiver[start..end]` — slicing (`recv`, `start`, `end`). A `str`
        /// receiver yields a `str` holding the bytes in `[start, end)`; an
        /// array receiver yields a fresh array of the elements in that range.
        /// Both bounds are clamped to `[0, len]` (out-of-range ends yield a
        /// shorter result; `start >= end` yields an empty one). A `str` result
        /// shares the source's backing buffer when possible (a copy for a
        /// small or SSO source); an array result copies (and retains) its
        /// elements. An open-ended `receiver[start..]` (end `None`) runs to
        /// the receiver's length — codegen fills it in, so no user-level
        /// `len` is needed.
        Slice(Box<Expr>, Box<Expr>, Option<Box<Expr>>),
        /// `expr?` — the error-propagation operator. `expr` must be a result
        /// `T!E`; in an `_!E`-returning function it evaluates to the `T` when
        /// `expr` is `ok`, and otherwise early-returns the `err(E)`.
        Try(Box<Expr>),
        /// The unit value `()` — the value of a statement-only block (one
        /// with no trailing expression). Has the unit type; users can't
        /// write it directly. It's how a function body that does work but
        /// produces nothing terminates.
        Unit,
        /// `expr; rest` — an expression statement: evaluate `expr` purely
        /// for its effects, discard its value (of any type), then evaluate
        /// and yield `rest`. This is how a void call like `print(x);` is
        /// sequenced ahead of the rest of a block.
        Seq(Box<Expr>, Box<Expr>),
        /// `return value;` — early-return from the enclosing function with
        /// `value` (whose type must match the function's return type). A
        /// statement (its own value is unit, like an assignment): control never
        /// falls through it, so anything after it in the block is unreachable.
        Return(Box<Expr>),
        /// `|x, y| body` — a lambda. Only valid as a call argument; the
        /// receiving function is monomorphized per lambda (the lambda is
        /// lifted to a synthesized function and captured variables passed in).
        /// Parameter types are usually inferred from the receiving function's
        /// signature, so they're optional.
        Lambda(Vec<LambdaParam>, Box<Expr>),
        /// `(a, b, c)` — a tuple literal of 2+ values. Lowered to
        /// `Construct(synth_struct_name, ..)` by mono's `infer` after element
        /// types are known; only the parser through the mono pass see this.
        TupleLit(Vec<Expr>),
        /// `name = expr` inside a call's argument list — a keyword argument.
        /// Only the parser and the loader ever see this: the loader's
        /// keyword-argument expansion resolves it against the callee's keyword
        /// parameters and rewrites the call to plain positional arguments
        /// (erroring on any misuse), so every later pass can treat it as
        /// unreachable.
        ///
        /// The `bool` is the `name?= expr` spelling: the argument is an
        /// *optional already*, passed through rather than wrapped. It is only
        /// valid against a [`Param::implicit_some`] parameter, whose call sites
        /// otherwise pass a bare `T` — see the `kwargs` module in `aipl-loader`.
        KwArg(String, Box<Expr>, bool),
        /// `f::<T, U>(args)` and `recv.f::<T, U>()` — a call with its type
        /// arguments written out: the wrapped expression is the call, and the
        /// types are the callee's declared type parameters, in declaration
        /// order, less any trailing ones that have a default. Only the parser through the checker see this: checking
        /// resolves the arguments into the call's recorded result type and
        /// stamps the call without the wrapper, so every later pass can treat
        /// it as unreachable.
        TypeArgs(Box<Expr>, Vec<Type>),
    }

    /// A lambda parameter: a name and an optional type annotation (inferred
    /// from the expected function type when omitted).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LambdaParam {
        pub name: String,
        pub ty: Option<Type>,
        pub span: Span,
    }

    /// The pattern of a `match` arm. An enum so the kinds are mutually exclusive
    /// (an arm can't be both a constructor and a literal).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Pattern {
        /// A constructor pattern — `Ctor(b0, b1, ...)`, a nullary `Ctor`, `none`,
        /// `some(v)`, `ok(v)`/`err(e)`, or a variant case — with its positional
        /// payload `bindings` (empty for a nullary case). The scrutinee's type
        /// decides which `name`s are legal.
        ///
        /// `ignore_payload` is the `Ctor(..)` form: the case carries a payload
        /// the arm doesn't look at, so nothing is bound and the arity isn't
        /// written out. It is *not* the same as the nullary `Ctor`, which claims
        /// the case has no payload at all and is still an error when it does —
        /// so a reader can tell "this case is empty" from "I'm ignoring what it
        /// holds" without knowing the variant. Implies `bindings.is_empty()`;
        /// monomorphization expands it to one `_` binder per payload slot
        /// (`expand_ignored_payload`), so codegen never sees the form.
        Ctor {
            name: String,
            bindings: Vec<String>,
            ignore_payload: bool,
        },
        /// A string-literal pattern `"lit" => body` (matches a `str` scrutinee by
        /// content).
        Str(String),
        /// A char-literal pattern `'c' => body` (matches a `char` scrutinee by
        /// value). Like [`Pattern::Str`] the domain is open, so such a match
        /// must end in a `_` arm.
        Char(u8),
        /// `[p0, p1, ...]` where at least one element is not a plain binder or
        /// a literal — `[n, Tok("?"), _, ty]`, `[some(v), none]`. The
        /// counterpart of [`Pattern::Array`] exactly as [`Pattern::Nested`] is
        /// of [`Pattern::Ctor`]: an array pattern whose elements *are* all
        /// plain is the simple form, which is what [`Pattern::normalized`]
        /// turns it into.
        ///
        /// Like the other nested forms it exists from the parser to
        /// monomorphization only: mono compiles it into a length test plus one
        /// column per element, so codegen only ever sees [`Pattern::Array`].
        ArrayNested(Vec<Pattern>),
        /// An array/`str`-destructuring pattern `[e0, e1, ...] => body` (matches a
        /// `str` or array scrutinee by exact length, then, per element position,
        /// either a bound name or a literal-equality check). Each element is a
        /// bare identifier — a **binder** for the element at that position (typed
        /// `char` for a `str`, the element type for an array) — or a literal
        /// expression matched by equality (validated by the checker). An
        /// all-literal pattern (e.g. `[1, 2]`) still introduces no bindings.
        Array(Vec<Expr>),
        /// `[e0, e1, ..]` — the same, but of an array or `str` of *at least* that
        /// many elements: the elements written are the first ones, and the `..`
        /// accepts whatever follows. Only the simple elements of [`Pattern::Array`]
        /// (a binder, `_`, or a literal) are taken, so this is compiled directly,
        /// as that is, with a length test of `>=` where `Array`'s is `==`.
        ArrayRest(Vec<Expr>),
        /// The wildcard / default arm `_ => body` (matches anything). Only valid
        /// for a `str` or array match, where it must be the last arm.
        Wildcard,

        // ---- Nested patterns ----
        //
        // The four below are what makes a pattern compose: a tuple of patterns,
        // a constructor whose payload slots are themselves patterns, an integer
        // literal, and a name binding whatever it matched. They exist from the
        // parser to monomorphization only. The checker types them and proves the
        // match exhaustive (see `check::usefulness`), and mono's `infer` compiles
        // an arm that uses any of them into a decision tree of the *simple*
        // forms above — a `Match` per constructor test, an `if` per literal
        // test, a `let` per binder — so codegen never sees them.
        /// `(p0, p1, ...)` — a tuple pattern, one sub-pattern per element. A
        /// tuple scrutinee is its synthetic struct, and each sub-pattern matches
        /// the corresponding `_i` field.
        Tuple(Vec<Pattern>),
        /// `Ctor(p0, p1, ...)` where at least one slot is not a plain binder —
        /// `some(1)`, `Pair(x, none)`, `ok((a, b))`. A constructor whose slots
        /// *are* all plain binders is the simple [`Pattern::Ctor`], which is
        /// what [`Pattern::normalized`] turns it into.
        Nested { name: String, args: Vec<Pattern> },
        /// An integer literal pattern `1 => ..`, matching a scalar by value. The
        /// domain is open, so a match on one needs a binder or `_` to be
        /// exhaustive.
        Int(i64),
        /// A name at a nested position, binding the value it matched. At the top
        /// of an arm a bare name is a nullary constructor, as it always was; in a
        /// nested slot it is a binder unless it spells a case — `none`, or a
        /// capitalised name — which is the rule [`Pattern::normalized`] applies,
        /// and the checker then confirms against the type.
        Bind(String),
    }

    impl Pattern {
        /// The positional binders this pattern introduces, in order: a
        /// constructor pattern's payload binders, or an array/`str` pattern's
        /// identifier elements (its literal elements bind nothing), or — for a
        /// nested pattern — every [`Pattern::Bind`] in it, depth first, left to
        /// right. Empty for a string-literal or wildcard pattern.
        pub fn bindings(&self) -> Vec<String> {
            match self {
                Pattern::Ctor { bindings, .. } => bindings.clone(),
                Pattern::Array(elems) | Pattern::ArrayRest(elems) => elems
                    .iter()
                    .filter_map(|e| match &e.kind {
                        ExprKind::Ident(name) => Some(name.clone()),
                        _ => None,
                    })
                    .collect(),
                Pattern::Str(_) | Pattern::Char(_) | Pattern::Wildcard | Pattern::Int(_) => {
                    Vec::new()
                }
                Pattern::Bind(name) => vec![name.clone()],
                Pattern::Tuple(ps)
                | Pattern::Nested { args: ps, .. }
                | Pattern::ArrayNested(ps) => ps.iter().flat_map(Pattern::bindings).collect(),
            }
        }

        /// This pattern with each binder replaced by `rename(binder)`, called once
        /// per binder in [`Pattern::bindings`] order. Literals and the wildcard
        /// bind nothing and are kept.
        pub fn rename_bindings(&self, rename: &mut impl FnMut(&str) -> String) -> Pattern {
            let elems = |elems: &[Expr], rename: &mut dyn FnMut(&str) -> String| {
                elems
                    .iter()
                    .map(|e| match &e.kind {
                        ExprKind::Ident(name) => Expr::rebuilt(ExprKind::Ident(rename(name)), e),
                        _ => e.clone(),
                    })
                    .collect::<Vec<_>>()
            };
            match self {
                Pattern::Ctor {
                    name,
                    bindings,
                    ignore_payload,
                } => Pattern::Ctor {
                    name: name.clone(),
                    bindings: bindings.iter().map(|b| rename(b)).collect(),
                    ignore_payload: *ignore_payload,
                },
                Pattern::Array(es) => Pattern::Array(elems(es, rename)),
                Pattern::ArrayRest(es) => Pattern::ArrayRest(elems(es, rename)),
                Pattern::Str(_) | Pattern::Char(_) | Pattern::Wildcard | Pattern::Int(_) => {
                    self.clone()
                }
                Pattern::Bind(name) => Pattern::Bind(rename(name)),
                Pattern::Tuple(ps) => {
                    Pattern::Tuple(ps.iter().map(|p| p.rename_bindings(rename)).collect())
                }
                Pattern::ArrayNested(ps) => {
                    Pattern::ArrayNested(ps.iter().map(|p| p.rename_bindings(rename)).collect())
                }
                Pattern::Nested { name, args } => Pattern::Nested {
                    name: name.clone(),
                    args: args.iter().map(|p| p.rename_bindings(rename)).collect(),
                },
            }
        }

        /// The constructor name for a `Ctor` (or nested constructor) pattern;
        /// `None` otherwise.
        pub fn ctor_name(&self) -> Option<&str> {
            match self {
                Pattern::Ctor { name, .. } | Pattern::Nested { name, .. } => Some(name),
                Pattern::Str(_)
                | Pattern::Char(_)
                | Pattern::Array(_)
                | Pattern::ArrayRest(_)
                | Pattern::ArrayNested(_)
                | Pattern::Wildcard
                | Pattern::Tuple(_)
                | Pattern::Int(_)
                | Pattern::Bind(_) => None,
            }
        }

        /// This pattern's elements as sub-patterns, for either array form —
        /// `None` when it is not an array pattern at all.
        ///
        /// The simple [`Pattern::Array`] holds expressions, so its elements are
        /// read back as the patterns they stand for: an identifier binds (or is
        /// `_`), a literal matches by value. That is what lets one `match` mix
        /// the two forms — `[a, b]` beside `[some(v), none]` — which it has to,
        /// since whether a given arm is plain is a property of that arm alone.
        pub fn array_elems(&self) -> Option<Vec<Pattern>> {
            match self {
                Pattern::ArrayNested(ps) => Some(ps.clone()),
                Pattern::Array(elems) => elems
                    .iter()
                    .map(|e| match &e.kind {
                        ExprKind::Ident(n) if n == "_" => Some(Pattern::Wildcard),
                        ExprKind::Ident(n) => Some(Pattern::Bind(n.clone())),
                        ExprKind::Str(s) => Some(Pattern::Str(s.clone())),
                        ExprKind::Num(n) => Some(Pattern::Int(*n)),
                        ExprKind::Char(c) => Some(Pattern::Char(*c)),
                        // The checker has already refused anything else.
                        _ => None,
                    })
                    .collect(),
                _ => None,
            }
        }

        /// Whether this pattern uses any of the nested forms — the ones only the
        /// checker and mono know, which an arm has to be compiled away from.
        pub fn is_nested(&self) -> bool {
            match self {
                Pattern::Tuple(_)
                | Pattern::Nested { .. }
                | Pattern::ArrayNested(_)
                | Pattern::Int(_)
                | Pattern::Bind(_) => true,
                Pattern::Ctor { .. }
                | Pattern::Str(_)
                | Pattern::Char(_)
                | Pattern::Array(_)
                | Pattern::ArrayRest(_)
                | Pattern::Wildcard => false,
            }
        }

        /// The pattern as the parser hands it over, in its canonical shape: a
        /// constructor whose slots are all plain binders is the simple
        /// [`Pattern::Ctor`], and a bare name in a nested slot is a
        /// [`Pattern::Bind`] unless it spells a case (`none`, or a capitalised
        /// name). `top` says whether this is the whole arm's pattern, where a
        /// bare name is always a nullary constructor.
        pub fn normalized(self, top: bool) -> Pattern {
            match self {
                Pattern::Nested { name, args } => {
                    let args: Vec<Pattern> =
                        args.into_iter().map(|p| p.normalized(false)).collect();
                    if args.is_empty() {
                        if !top && is_binder_name(&name) {
                            return Pattern::Bind(name);
                        }
                        return Pattern::Ctor {
                            name,
                            bindings: Vec::new(),
                            ignore_payload: false,
                        };
                    }
                    // Plain binders and `_` only: the simple form, with `_` as
                    // the binder that names nothing — what `Right(_)` always was.
                    if args
                        .iter()
                        .all(|p| matches!(p, Pattern::Bind(_) | Pattern::Wildcard))
                    {
                        return Pattern::Ctor {
                            name,
                            bindings: args
                                .into_iter()
                                .map(|p| match p {
                                    Pattern::Bind(n) => n,
                                    _ => "_".to_string(),
                                })
                                .collect(),
                            ignore_payload: false,
                        };
                    }
                    Pattern::Nested { name, args }
                }
                Pattern::Tuple(ps) => {
                    Pattern::Tuple(ps.into_iter().map(|p| p.normalized(false)).collect())
                }
                Pattern::ArrayNested(ps) => {
                    Pattern::ArrayNested(ps.into_iter().map(|p| p.normalized(false)).collect())
                }
                // A bare name the parser could only read as a nullary
                // constructor is a binder in a nested slot.
                Pattern::Ctor {
                    name,
                    bindings,
                    ignore_payload: false,
                } if !top && bindings.is_empty() && is_binder_name(&name) => Pattern::Bind(name),
                other => other,
            }
        }
    }

    /// Whether a bare name in a nested pattern slot binds rather than names a
    /// case: everything but `none` and a capitalised name. Cases are
    /// capitalised by convention and `none` is a keyword, so this is the one
    /// rule that needs no type to apply — the checker confirms it against the
    /// scrutinee.
    pub fn is_binder_name(name: &str) -> bool {
        name != "none" && !name.starts_with(|c: char| c.is_ascii_uppercase())
    }

    /// One arm of a `match`: a [`Pattern`] and its body.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MatchArm {
        pub pattern: Pattern,
        pub body: Expr,
        pub span: Span,
    }
}

use ast::{BinOp, Callee, Primitive, Type};
use std::collections::HashMap;

// ---------- Shared AST-level `Type` helpers ----------
//
// These operate purely on `ast::Type` (never on cranelift types), so they
// live here in the base crate where the parser, monomorphizer, codegen, and
// loader can all reach them without depending on each other.

/// The fixed-width integer types: signed `i8`/`i16`/`i32`/`i64` and unsigned
/// `u8`/`u16`/`u32`/`u64`. All are scalars; `i64` is the default for integer
/// literals.
pub const INT_TYPES: &[&str] = &["i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64"];

pub fn is_int_ty(t: &Type) -> bool {
    matches!(t, Type::Primitive(p) if p.is_int())
}

/// Whether a value of type `from` widens losslessly to `to` — both integer
/// types, per [`Primitive::widens_to`]. Only the scalar itself: an `u8[]` is not
/// an `i64[]` (narrow elements are packed at their own width) and a `(i64) ->
/// bool` is not a `(u8) -> bool`.
pub fn int_widens(from: &Type, to: &Type) -> bool {
    matches!((from, to), (Type::Primitive(a), Type::Primitive(b)) if a.widens_to(*b))
}

/// Bit width of an integer type *name* (`"i8"` → 8, `"u64"` → 64), or `None` if
/// `name` isn't an integer type. The name-based form is for callers that only
/// have a spelling (the lexer, a called conversion-fn name); given a
/// [`Primitive`] use [`Primitive::int_bits`] directly.
pub fn int_bits(name: &str) -> Option<u32> {
    Primitive::from_name(name).and_then(Primitive::int_bits)
}

/// Whether an integer type *name* is signed (`i*`) vs unsigned (`u*`). See
/// [`int_bits`] on the name-vs-[`Primitive`] split.
pub fn int_signed(name: &str) -> bool {
    Primitive::from_name(name).is_some_and(Primitive::int_signed)
}

/// If `e` is a compile-time integer constant — an integer literal, possibly
/// negated — return its value. Used to let a bare literal flow into a narrow
/// integer context (e.g. `i8_val == 5`) without an explicit conversion, with a
/// range check (see [`int_fits`]).
pub fn const_int(e: &ast::Expr) -> Option<i64> {
    match &e.kind {
        ast::ExprKind::Num(n) => Some(*n),
        ast::ExprKind::Neg(inner) => const_int(inner).map(i64::wrapping_neg),
        _ => None,
    }
}

/// Collect every integer value a *purely literal* expression can produce into
/// `out`, returning whether `e` is one. An integer literal keeps its
/// flexibility through the constructs that merely pass a value along — a
/// block's tail, a binding's body, an assignment's continuation, and every arm
/// of an `if`/`match` — so
///
/// ```text
/// fn f() -> u8 { let x = compute(); 200 }
/// fn g(b: bool) -> u8 { if (b) { 1 } else { 200 } }
/// ```
///
/// flex exactly like the bare `fn f() -> u8 { 200 }`. Every branch must itself
/// be literal (one non-literal arm makes the whole expression non-flexible, and
/// the ordinary merge rules take over), and *all* collected values must fit the
/// target — checked by the caller, so the diagnostic can name the offender.
pub fn flex_int_values(e: &ast::Expr, out: &mut Vec<i64>) -> bool {
    match &e.kind {
        ast::ExprKind::Num(_) | ast::ExprKind::Neg(_) => match const_int(e) {
            Some(v) => {
                out.push(v);
                true
            }
            None => false,
        },
        // Value-passing wrappers: only the tail decides the type.
        ast::ExprKind::Seq(_, tail)
        | ast::ExprKind::Let(_, _, _, tail)
        | ast::ExprKind::LetMut(_, _, _, tail)
        | ast::ExprKind::Assign(_, _, tail) => flex_int_values(tail, out),
        // Every branch must be literal for the whole to stay flexible.
        ast::ExprKind::If(_, a, b) => flex_int_values(a, out) && flex_int_values(b, out),
        ast::ExprKind::Match(_, arms) => {
            !arms.is_empty() && arms.iter().all(|arm| flex_int_values(&arm.body, out))
        }
        _ => false,
    }
}

/// Whether the integer value `v` (an `i64` literal) is representable in integer
/// type `name`. `u64` accepts any non-negative value (a literal can't exceed
/// `i64::MAX`, which fits `u64`). Computed by the dogfooded AIPL `int_fits` via
/// the embedding FFI (see [`set_int_fits_hook`]) — no native fallback.
pub fn int_fits(v: i64, name: &str) -> bool {
    INT_FITS_HOOK.get().expect(
        "int_fits hook not installed before checking an integer literal \
         (call install_parser_hooks first)",
    )(v, name)
}

/// The hook called by [`int_fits`] to range-check a flexible integer literal.
/// Installed by the compiler via [`set_int_fits_hook`] (the dogfooded AIPL
/// `int_fits`, run through the embedding FFI). No native fallback — panics if not
/// installed.
static INT_FITS_HOOK: std::sync::OnceLock<fn(i64, &str) -> bool> = std::sync::OnceLock::new();

/// Install the int-fits hook (the dogfooded AIPL `int_fits`, run through the
/// embedding FFI). Idempotent — first install wins. Must be called before any
/// check that range-flexes an integer literal (i.e. before `install_parser_hooks`
/// returns).
pub fn set_int_fits_hook(f: fn(i64, &str) -> bool) {
    let _ = INT_FITS_HOOK.set(f);
}

/// Can literal expression `e` (inferred as `ety`) flex to `target`, and to what?
///
/// `Ok(Some(t))` — yes, `e` takes type `t`. `Ok(None)` — not a flexible shape,
/// leave it to the ordinary coercion rules. `Err((value, type_name))` — the
/// shape matched but `value` doesn't fit `type_name`, which is a hard error the
/// caller renders (the checker points at the offending literal).
///
/// Flexing is *structural*: it reaches through the constructs that merely pass a
/// value along (a block tail, `if`/`match` arms) and, crucially, **into
/// container literals**. An array/set/dict literal or a `some(..)` flexes when
/// every literal inside it flexes to the corresponding part of the target, so
///
/// ```text
/// let xs: u8[] = [200, 100];
/// let o: u8? = some(200);
/// let d: #{str: u8} = #{"a": 200};
/// ```
///
/// work exactly like the scalar `let n: u8 = 200`. Without this, an untyped
/// `[200]` is stuck at `i64[]` and a narrow-element array could only be built by
/// converting each element one at a time.
///
/// Retyping alone is sufficient: an integer literal that fits its target is
/// *already* canonical in its i64 register (sign-extended for `i*`, masked for
/// `u*`), and an element slot is 8 bytes, so nothing about the emitted value
/// changes — only its static type.
pub fn flex_fit(
    e: &ast::Expr,
    ety: &Type,
    target: &Type,
) -> Result<Option<Type>, (i64, &'static str)> {
    use ast::ExprKind as K;
    // A constructor reference where a case is wanted *is* that case. The target
    // is what distinguishes the two readings of `Str`: here it is the tag, and
    // anywhere else it stays the value or the constructor function.
    if let Type::Case(want) = target {
        if let Some((_, variant)) = ctor_ref_case(e) {
            let ok = match &**want {
                // An unresolved variable is what a generic payload looks like
                // (`Term`'s `Case<K>` before `K` is pinned) — the reference is
                // what pins it.
                Type::TypeVar(_) => true,
                Type::Named(n) => n == variant,
                _ => false,
            };
            if ok {
                return Ok(Some(Type::Case(Box::new(Type::Named(variant.to_string())))));
            }
        }
    }
    // Value-passing wrappers: only the tail decides the type. Mirrors
    // `flex_int_values`, so `{ let a = f(); [200] }` flexes like a bare `[200]`.
    match &e.kind {
        K::Seq(_, tail)
        | K::Let(_, _, _, tail)
        | K::LetMut(_, _, _, tail)
        | K::Assign(_, _, tail) => {
            return flex_fit(tail, ety, target);
        }
        K::If(_, a, b) => {
            // Both branches must flex, and to the same target.
            return Ok(
                match (flex_fit(a, ety, target)?, flex_fit(b, ety, target)?) {
                    (Some(t), Some(_)) => Some(t),
                    _ => None,
                },
            );
        }
        _ => {}
    }
    match target {
        // The scalar case: a bare (possibly negated) integer literal.
        Type::Primitive(p) if p.is_int() => {
            if ety == target {
                return Ok(None);
            }
            let mut vs = Vec::new();
            if !flex_int_values(e, &mut vs) {
                return Ok(None);
            }
            match vs.iter().find(|v| !int_fits(**v, p.name())) {
                Some(v) => Err((*v, p.name())),
                None => Ok(Some(target.clone())),
            }
        }
        // A container literal flexes when every element does. An *empty* literal
        // is left alone: it already coerces through the `__none__`/empty-arg
        // markers, and there is nothing inside to range-check.
        Type::Array(inner) => match &e.kind {
            K::ArrayLit(elems) if !elems.is_empty() => flex_all(elems.iter(), inner, target),
            _ => Ok(None),
        },
        // A set literal flexes only into a set of its own order: the order is
        // part of the type, and retyping `#<{1}` as a `#>{i64}` would hand a
        // block sorted one way to code reading it the other.
        Type::Set(inner, order) => match &e.kind {
            K::SetLit(elems, lit_order) if !elems.is_empty() && lit_order == order => {
                flex_all(elems.iter(), inner, target)
            }
            _ => Ok(None),
        },
        Type::Dict(kt, vt) => match &e.kind {
            K::DictLit(entries) if !entries.is_empty() => {
                let mut flexed = false;
                for (k, v) in entries {
                    // A dict flexes if *either* half does — a `#{str: u8}` has a
                    // non-flexing key and a flexing value.
                    flexed |= flex_fit(k, &Type::Unit, kt)?.is_some();
                    flexed |= flex_fit(v, &Type::Unit, vt)?.is_some();
                }
                Ok(flexed.then(|| target.clone()))
            }
            _ => Ok(None),
        },
        // `some(x)` — the optional's payload flexes to the core type.
        Type::Optional(inner) => match &e.kind {
            K::Call(Callee::Some, args, _) if args.len() == 1 => {
                Ok(flex_fit(&args[0], &Type::Unit, inner)?.map(|_| target.clone()))
            }
            _ => Ok(None),
        },
        // `ok(x)` / `err(x)` — the payload flexes to the side it lands on. Only
        // that side is examined: the other is `__none__` and coerces to whatever
        // the target declares, which is what already lets `ok(5)` satisfy a
        // `i64!str` return. Without this, `ok(0)` in a `-> u64!E` function typed
        // its payload `i64` and had to be written as a pre-annotated binding.
        //
        // Like the `some` arm above, this matches on the name alone — there is
        // no environment here to confirm `ok`/`err` are the constructors rather
        // than a local shadowing them. A shadowing binding would only cost the
        // flex, since the ordinary check still runs afterwards.
        Type::Result(ok, err) => match &e.kind {
            K::Call(name @ (Callee::Ok | Callee::Err), args, _) if args.len() == 1 => {
                let side = if *name == Callee::Ok { ok } else { err };
                Ok(flex_fit(&args[0], &Type::Unit, side)?.map(|_| target.clone()))
            }
            _ => Ok(None),
        },
        // `T without X`: a literal that is visibly not `X` takes the refined
        // type — the one way into a refinement that needs no runtime test.
        // The literal itself still flexes against the base (`let xs: u8[]
        // without [] = [200]`), and keeps its own type where nothing flexed, so
        // an element of the wrong type is still caught by the ordinary check
        // that follows: the refinement is granted, not the base.
        Type::Without(base, exclusion) => match exclusion {
            ast::Exclusion::EmptyArray => match &e.kind {
                K::ArrayLit(elems) if !elems.is_empty() => {
                    let base_ty = flex_fit(e, ety, base)?.unwrap_or_else(|| ety.clone());
                    Ok(Some(Type::Without(Box::new(base_ty), exclusion.clone())))
                }
                _ => Ok(None),
            },
            // `V without C`: a construction of some other case is visibly not
            // a `C`. The value keeps the type it has — a `V` — and is granted
            // the refinement on top of it.
            ast::Exclusion::NamedCase(excluded) => match constructed_case(e) {
                Some((case, _)) if case != excluded => {
                    let base_ty = flex_fit(e, ety, base)?.unwrap_or_else(|| ety.clone());
                    Ok(Some(Type::Without(Box::new(base_ty), exclusion.clone())))
                }
                _ => Ok(None),
            },
        },
        _ => Ok(None),
    }
}

/// The container takes `whole` when at least one element flexes to `inner` and
/// none *fails* to fit it.
///
/// Element types aren't known here, so each is probed with the unknown marker.
/// A non-literal element reports "doesn't flex", which is not a veto — it is
/// usually already the right type, as in `[1, u64_max()]` where the literal
/// needs the flex and the call does not. Mistyped elements are still caught: the
/// ordinary element check runs afterwards either way. An out-of-range literal is
/// a hard error and propagates.
fn flex_all<'a>(
    mut elems: impl Iterator<Item = &'a ast::Expr>,
    inner: &Type,
    whole: &Type,
) -> Result<Option<Type>, (i64, &'static str)> {
    let mut any = false;
    for e in &mut elems {
        any |= flex_fit(e, &Type::Unit, inner)?.is_some();
    }
    Ok(any.then(|| whole.clone()))
}

/// Retype a literal expression `e` (currently `ety`) to `other` — used by
/// mono/codegen after the checker has verified every literal involved fits, so a
/// literal's value flows into a narrow-int context without an explicit
/// conversion. The codegen-side mirror of the checker's `flex_int`; a
/// non-flexing shape or an out-of-range literal (already rejected upstream) is
/// left unchanged.
pub fn flex_int_ty(e: &ast::Expr, ety: &Type, other: &Type) -> Type {
    match flex_fit(e, ety, other) {
        Ok(Some(t)) => t,
        _ => ety.clone(),
    }
}

/// Every named operator builtin: `(builtin name, operator spelling, canonical
/// impl)`. The operator spelling is what the name must be aliased to on import
/// (`import { wrapping_add as + } from builtins;`), and the canonical impl is
/// the reserved `__builtin_*` function a use of that operator resolves to
/// (intrinsified by codegen).
///
/// Several builtins may provide the same operator (`wrapping_add`/
/// `saturating_add` both give `+`), which is what lets a file pick the semantics
/// it wants: the operator use dispatches on the resolved impl, not the spelling.
/// And several may share one impl — `wrapping_increment` is `++` over the same
/// `__builtin_wrapping_add` that `wrapping_add` gives `+`, since `set n++;` is
/// an add of `1`. That shared impl is exactly what makes two spellings
/// comparable: the increment lint pairs a file's `+` with the `++` of matching
/// semantics by looking for the flavor with the same impl.
///
/// This table is the single place operator builtins are declared — extend it
/// (not per-operator special-cases) to add flavors.
const OPERATOR_BUILTINS: &[(&str, &str, Callee)] = &[
    ("wrapping_add", "+", Callee::WrappingAdd),
    ("saturating_add", "+", Callee::SaturatingAdd),
    ("wrapping_sub", "-", Callee::WrappingSub),
    ("saturating_sub", "-", Callee::SaturatingSub),
    ("wrapping_mul", "*", Callee::WrappingMul),
    ("wrapping_increment", "++", Callee::WrappingAdd),
    ("saturating_increment", "++", Callee::SaturatingAdd),
    ("wrapping_decrement", "--", Callee::WrappingSub),
    ("saturating_decrement", "--", Callee::SaturatingSub),
    // The compound assignments, `set n += e;`. Each is its own named builtin
    // rather than a second alias of the plain one, exactly as `++` is
    // `wrapping_increment` and not `wrapping_add`: a name maps to one operator
    // here, and the separate name is what a file's import list shows.
    //
    // Where the plain operator has a callable `__builtin_*` impl the compound
    // form shares it, which is what lets the lint pair a file's `+` with the
    // `+=` of matching semantics (the same trick `matching_increment` uses).
    // `/=` has nothing to share — `saturating_divide`'s canonical is the marker
    // `/` — so its own spelling is the marker, and the loader collapses the
    // variant to `BinOp::Div` after gating.
    ("wrapping_add_assign", "+=", Callee::WrappingAdd),
    ("saturating_add_assign", "+=", Callee::SaturatingAdd),
    ("wrapping_sub_assign", "-=", Callee::WrappingSub),
    ("saturating_sub_assign", "-=", Callee::SaturatingSub),
    ("wrapping_mul_assign", "*=", Callee::WrappingMul),
    ("saturating_divide_assign", "/=", Callee::SaturatingDivide),
    // The operators with a single semantics. They are listed here so that *every*
    // operator is imported the same way when it is imported *as an operator*:
    // `name as op`. Reading an import list then tells you which operators a file
    // uses and, where it matters, which flavor, without the reader having to
    // know which operators happen to be ambiguous.
    //
    // They have only one flavor, so there is nothing for the view to dispatch
    // between — but the canonical is still a real `__builtin_*`, exactly as the
    // multi-flavor operators' are. That uniformity is the point: **after import
    // resolution an operator use is an ordinary call**, and no pass downstream of
    // the loader has to know an operator was involved. These canonicals used to be
    // the operator spelling itself — a *marker* rather than a callable name — which
    // is what forced the loader to lower a bare `concat(a, b)` back into a
    // primitive node so the call and the operator stayed one program. With a real
    // canonical both spellings simply resolve to the same callee and that special
    // case is gone.
    ("equal", "==", Callee::Equal),
    ("not_equal", "!=", Callee::NotEqual),
    ("less_than", "<", Callee::LessThan),
    ("greater_than", ">", Callee::GreaterThan),
    ("less_than_or_equal", "<=", Callee::LessThanOrEqual),
    ("greater_than_or_equal", ">=", Callee::GreaterThanOrEqual),
    ("logical_and", "&&", Callee::LogicalAnd),
    ("logical_or", "||", Callee::LogicalOr),
    ("logical_not", "!", Callee::LogicalNot),
    ("concat", "+++", Callee::Concat),
    ("saturating_divide", "/", Callee::SaturatingDivide),
    ("saturating_remainder", "%", Callee::SaturatingRemainder),
];

/// The single [`BinOp`] a canonical `__builtin_*` impl performs, for the
/// operators that have exactly one — the comparisons, the logical pair,
/// concatenation, and saturating divide/remainder.
///
/// This is the one place a resolved operator call is turned back into an opcode,
/// and it is deliberately *not* total over the operator builtins: the arithmetic
/// impls are excluded because `__builtin_wrapping_add` and
/// `__builtin_saturating_add` would both answer [`BinOp::Add`], losing the flavor
/// that is the whole reason they are separate names. Those keep their own
/// dispatch (in mono and codegen), keyed on the canonical name.
///
/// `None` for `__builtin_logical_not`, which is unary.
pub fn binop_for_builtin(canonical: &Callee) -> Option<BinOp> {
    Some(match canonical {
        Callee::Equal => BinOp::Eq,
        Callee::NotEqual => BinOp::Ne,
        Callee::LessThan => BinOp::Lt,
        Callee::GreaterThan => BinOp::Gt,
        Callee::LessThanOrEqual => BinOp::Le,
        Callee::GreaterThanOrEqual => BinOp::Ge,
        Callee::LogicalAnd => BinOp::And,
        Callee::LogicalOr => BinOp::Or,
        Callee::Concat => BinOp::Concat,
        Callee::SaturatingDivide => BinOp::Div,
        Callee::SaturatingRemainder => BinOp::Rem,
        _ => return None,
    })
}

/// A set in a generic signature's return type takes the order of the set the
/// call was made on: `union(self: #{T}, other: #{T}) -> #{T}` is written once,
/// for every order, and `#>{1}.union(#>{2})` has to come back a `#>{i64}`.
///
/// So `#{T}` in a signature is order-agnostic — a parameter accepts any set
/// (unification ignores the order; see `collect_var_bindings`), and a return
/// inherits from the argument bound to the first set-typed parameter. `ret` is
/// the substituted return type; `params` and `args` are the signature's
/// parameter types and the call's argument types, aligned by position.
pub fn inherit_set_order(ret: Type, params: &[Type], args: &[Type]) -> Type {
    let Type::Set(inner, ast::SetOrder::Unordered) = &ret else {
        return ret;
    };
    let from = params
        .iter()
        .zip(args)
        .find(|(p, _)| matches!(p, Type::Set(..)))
        .and_then(|(_, a)| match a {
            Type::Set(_, order) => Some(*order),
            _ => None,
        });
    match from {
        Some(order) => Type::Set(inner.clone(), order),
        None => ret,
    }
}

/// The right operand of `name +++ r` — the shape `set a = a +++ b;` takes, where
/// the append can go into `a`'s own buffer instead of building a fresh string.
///
/// Two passes have to agree on this shape exactly: mono decides whether the
/// binding is `exclusive`, and codegen emits the in-place append. Either one
/// alone leaves the optimization off. They were two hand-written matches once,
/// written against the old `char` opcode encoding and naming *addition* rather
/// than concatenation — so neither ever fired, and nothing noticed. One function,
/// asked twice, cannot disagree with itself.
pub fn self_append<'a>(value: &'a ast::Expr, name: &str) -> Option<&'a ast::Expr> {
    let ast::ExprKind::Call(f, args, _) = &value.kind else {
        return None;
    };
    let [l, r] = args.as_slice() else {
        return None;
    };
    (binop_for_builtin(f) == Some(BinOp::Concat)
        && matches!(&l.kind, ast::ExprKind::Ident(n) if n == name))
    .then_some(r)
}

/// How many operands the canonical `__builtin_*` impl of an operator takes, or
/// `None` when the name is not one. Used to check the arity of a bare
/// operator-builtin *call* (`concat(a, b)`), which no signature covers because
/// these are intrinsified rather than emitted.
pub fn operator_arity(canonical: &Callee) -> Option<usize> {
    match canonical {
        Callee::LogicalNot => Some(1),
        Callee::WrappingAdd
        | Callee::SaturatingAdd
        | Callee::WrappingSub
        | Callee::SaturatingSub
        | Callee::WrappingMul => Some(2),
        _ => binop_for_builtin(canonical).map(|_| 2),
    }
}

/// The named builtins that provide `op`, in [`OPERATOR_BUILTINS`] order —
/// non-empty exactly when `op` is an operator spelling, which is how the lints
/// tell an operator call from a named one.
///
/// A bare operator *import* is refused before the loader sees it, by the
/// parser (`bare_operator_import` in `grammar_aipl.aipl`, reading the same
/// pairing from the dogfooded `operator_named_forms.aipl`), so nothing here
/// generates that message any more.
pub fn operator_named_forms(op: &str) -> Vec<&'static str> {
    OPERATOR_BUILTINS
        .iter()
        .filter(|(_, o, _)| *o == op)
        .map(|(n, _, _)| *n)
        .collect()
}

/// If `name` is a named operator builtin, the `(operator, canonical impl)` it
/// provides — see [`OPERATOR_BUILTINS`].
pub fn operator_builtin(name: &str) -> Option<(&'static str, &'static Callee)> {
    OPERATOR_BUILTINS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, op, canonical)| (*op, canonical))
}

/// The operator builtin providing `op` with the semantics of `canonical_impl` —
/// the inverse of [`operator_builtin`], for naming the import that pairs with
/// one a file already has (`__builtin_saturating_add` + `"++"` →
/// `saturating_increment`).
pub fn operator_builtin_named(op: &str, canonical_impl: &Callee) -> Option<&'static str> {
    OPERATOR_BUILTINS
        .iter()
        .find(|(_, o, c)| *o == op && c == canonical_impl)
        .map(|(name, _, _)| *name)
}

/// Whether `s` spells a built-in operator that must be imported to be used
/// (e.g. `import { equal as == } from builtins;`; `+` comes via `wrapping_add as +`).
/// Computed by the dogfooded AIPL `is_operator_name` via the embedding FFI (see
/// [`set_is_operator_name_hook`]) — no native fallback.
pub fn is_operator_name(s: &str) -> bool {
    IS_OPERATOR_NAME_HOOK.get().expect(
        "is_operator_name hook not installed before resolving an operator import \
         (call install_parser_hooks first)",
    )(s)
}

/// The hook called by [`is_operator_name`]. Installed by the compiler via
/// [`set_is_operator_name_hook`] (the dogfooded AIPL `is_operator_name`, run
/// through the embedding FFI). No native fallback — panics if not installed.
static IS_OPERATOR_NAME_HOOK: std::sync::OnceLock<fn(&str) -> bool> = std::sync::OnceLock::new();

/// Install the is-operator-name hook (the dogfooded AIPL `is_operator_name`, run
/// through the embedding FFI). Idempotent — first install wins. Must be called
/// before any operator-import resolution (i.e. before `install_parser_hooks`
/// returns).
pub fn set_is_operator_name_hook(f: fn(&str) -> bool) {
    let _ = IS_OPERATOR_NAME_HOOK.set(f);
}

/// What a function's name says it is — see `fn_names.aipl`, which owns the
/// naming scheme for the functions the parser synthesizes from a declaration's
/// attributes and the questions asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FnRole {
    /// A name a source file can spell.
    Plain,
    /// Exactly a `.test` body (`__test$<f>`).
    TestBody,
    /// A function hoisted out of a `.test` block, with the name it was
    /// declared with.
    TestHelper(String),
    /// The function standing for `<source>`'s `.performance` block.
    Performance(String),
}

/// The hooks into `fn_names.aipl`, installed by the compiler via
/// [`set_fn_names_hooks`]. No native fallback — each panics if not installed.
struct FnNamesHooks {
    role: fn(&str) -> FnRole,
    test_fn_name: fn(&str) -> String,
    performance_fn_name: fn(&str) -> String,
}

static FN_NAMES_HOOKS: std::sync::OnceLock<FnNamesHooks> = std::sync::OnceLock::new();

/// Install the function-name hooks: the dogfooded AIPL `fn_role`,
/// `test_fn_name` and `performance_fn_name`, run through the embedding FFI.
/// Idempotent — first install wins.
pub fn set_fn_names_hooks(
    role: fn(&str) -> FnRole,
    test_fn_name: fn(&str) -> String,
    performance_fn_name: fn(&str) -> String,
) {
    let _ = FN_NAMES_HOOKS.set(FnNamesHooks {
        role,
        test_fn_name,
        performance_fn_name,
    });
}

fn fn_names_hooks() -> &'static FnNamesHooks {
    FN_NAMES_HOOKS
        .get()
        .expect("function-name hooks not installed (call install_parser_hooks)")
}

/// What `name` says its function is — see [`FnRole`].
pub fn fn_role(name: &str) -> FnRole {
    (fn_names_hooks().role)(name)
}

/// The name of the function a `.test` block on `f` becomes — `__test$<f>` — and
/// the test that a function *is* one ([`is_test_body`]). A test body's name
/// changes what `?` means inside it (an `err` fails the current test rather
/// than returning), so the test must match exactly the bodies this produced.
pub fn test_fn_name(f: &str) -> String {
    (fn_names_hooks().test_fn_name)(f)
}

/// The name of the function standing for `f`'s `.performance` block. The parser
/// reads that attribute off `f` and turns it into a function of its own, whose
/// `.test` body is the block — so a performance block reaches everything
/// downstream as the test code it currently is.
pub fn performance_fn_name(f: &str) -> String {
    (fn_names_hooks().performance_fn_name)(f)
}

/// The function `name` stands for the `.performance` block of, or `None` when
/// `name` is not one — see [`performance_fn_name`].
pub fn performance_source_name(name: &str) -> Option<&str> {
    match fn_role(name) {
        // The source name is what follows the prefix, so it is a suffix of `name`.
        FnRole::Performance(source) => Some(&name[name.len() - source.len()..]),
        FnRole::Plain | FnRole::TestBody | FnRole::TestHelper(_) => None,
    }
}

/// The name of the function that computes the top-level constant `name`:
/// `__const$<name>`. A constant whose value is not a literal is a nullary
/// function of this name, and each use is a call to it that codegen answers
/// from a cache, so the body runs once per thread, on first use.
pub fn constant_initializer_name(name: &str) -> String {
    format!("{CONSTANT_INITIALIZER_PREFIX}{name}")
}

/// Whether `name` is a function made by [`constant_initializer_name`]. A `$`
/// cannot appear in a source name, so no program spells one.
pub fn is_constant_initializer(name: &str) -> bool {
    name.starts_with(CONSTANT_INITIALIZER_PREFIX)
}

const CONSTANT_INITIALIZER_PREFIX: &str = "__const$";

/// Whether `name` is exactly a `.test` body — see [`test_fn_name`].
pub fn is_test_body(name: &str) -> bool {
    matches!(fn_role(name), FnRole::TestBody)
}

/// The name a hoisted test helper was declared with, or `None` when `name` is
/// not one.
pub fn test_helper_source_name(name: &str) -> Option<&str> {
    match fn_role(name) {
        // The declared name is what follows the last `$`, so it is a suffix.
        FnRole::TestHelper(helper) => Some(&name[name.len() - helper.len()..]),
        FnRole::Plain | FnRole::TestBody | FnRole::Performance(_) => None,
    }
}

/// Whether `name` is a hoisted test helper. Test code like the body it came
/// from, so `assert` is baked in it too.
pub fn is_test_helper(name: &str) -> bool {
    test_helper_source_name(name).is_some()
}

/// The loader file index encoded in a mangled `__m<N>__name`, or `None` when the
/// name carries no such prefix — a root-file item (the loader leaves those
/// unmangled) or a builtin declaration.
///
/// The inverse direction of the loader's `mangle`, kept here because the passes
/// that need it (attributing a diagnostic to its file, via
/// [`ast::Program::sources`]) live downstream of the loader and cannot call into
/// it.
pub fn mangled_file_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("__m")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !rest[digits.len()..].starts_with("__") {
        return None;
    }
    digits.parse().ok()
}

/// Every `trace(expr)` left in `program`, as a diagnostic apiece, attributed to
/// the file it was written in.
///
/// `trace` prints without declaring an effect, which is what makes it usable
/// mid-debugging and what makes it unacceptable in anything kept. The two
/// commands that produce something to keep refuse it, and `command` is which one
/// is asking:
///
/// - `aipl check` runs a file's tests and *then* fails on this. Running first is
///   the point: a trace hit by a test has already printed by the time its
///   diagnostic appears.
/// - `aipl build` refuses before writing the executable, since a trace in a
///   linked binary is the thing this is all meant to prevent.
///
/// `aipl run` deliberately does **not** — running a program to watch what it does
/// is the case `trace` exists for.
///
/// Spans point at the traced expression, not the whole call, so the caret
/// underlines what was being printed.
pub fn trace_diagnostics(program: &ast::Program, command: &str) -> Vec<Error> {
    let mut out = Vec::new();
    for item in &program.items {
        let ast::Item::Fn(f) = item else { continue };
        for body in [Some(&f.body), f.test_body.as_ref()].into_iter().flatten() {
            each_subexpr(body, &mut |e| {
                if matches!(&e.kind, ast::ExprKind::Call(c, _, _) if *c == ast::Callee::Trace) {
                    let e = Error::at(
                        format!(
                            "`trace(..)` prints for debugging and declares no effect, so \
                             `{command}` refuses a program that still contains one; remove it"
                        ),
                        e.span.clone(),
                    );
                    out.push(attribute_to_file(e, &f.name.text, &program.sources));
                }
            });
        }
    }
    out
}

/// The ways a `.performance` block is wrong that its types and effects do not
/// already say: a literal measurement label used twice in one block.
///
/// A label names a measurement in a report, so two with the same text could not
/// be told apart. Only literal labels are compared — a computed one (`` `n={n}` ``
/// in a sweep) has no text until it runs, and is the caller's to keep distinct.
/// Labels are per block: two blocks may each have a `"small"`.
///
/// Reported by `aipl check` before the block runs, the way a type error is.
pub fn performance_diagnostics(program: &ast::Program) -> Vec<Error> {
    let mut out = Vec::new();
    for item in &program.items {
        let ast::Item::Fn(f) = item else { continue };
        if performance_source_name(&f.name).is_none() {
            continue;
        }
        let Some(block) = f.test_body.as_ref() else {
            continue;
        };
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        each_subexpr(block, &mut |e| {
            let ast::ExprKind::Call(callee, args, _) = &e.kind else {
                return;
            };
            if *callee != ast::Callee::Measure {
                return;
            }
            let Some(ast::ExprKind::Str(label)) = args.first().map(|a| &a.kind) else {
                return;
            };
            if !seen.insert(label.clone()) {
                let e = Error::at(
                    format!(
                        "duplicate measurement label {label:?} in this `.performance` block — a report could not tell the two apart"
                    ),
                    args[0].span.clone(),
                );
                out.push(attribute_to_file(e, &f.name.text, &program.sources));
            }
        });
    }
    out
}

/// Tag `e` with the source of the file the function named `name` came from, so
/// the caret lands in *that* file rather than in whatever source the caller
/// holds.
///
/// The post-load half of the loader's `tag_origin`, and the same trick
/// `aipl-mono`'s checker plays: after flattening, a function's origin survives
/// only in the `__m<N>__` its name was mangled with. A root-file item carries no
/// such prefix and is already rendered correctly by its caller, so it is left
/// alone.
fn attribute_to_file(e: Error, name: &str, sources: &[ast::FileSource]) -> Error {
    let Some(idx) = mangled_file_index(name) else {
        return e;
    };
    match sources.get(idx as usize) {
        Some(f) if !f.source.is_empty() => e.in_file(&f.label, &f.source),
        _ => e,
    }
}

/// The canonical operator spelling for a [`BinOp`] — what a diagnostic prints,
/// and what the operator-import gate looks up. This is the *only* place the
/// mapping from operator to spelling lives; a pass that wants to know which
/// operator it is asks the enum, not this string.
///
/// Exhaustive on purpose: a new operator will not compile until it is given a
/// spelling here, where the old `char` version had a `_ => "?"` fallthrough that
/// silently rendered an unknown opcode as a question mark. Unary `Neg`/`Not`
/// spell `-`/`!` and are not [`BinOp`]s.
pub fn binop_spelling(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Le => "<=",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Concat => "+++",
    }
}

/// Collect the spellings of every operator used anywhere in `e` (for the
/// operator-import migration tooling).
pub fn collect_operators(e: &ast::Expr, out: &mut std::collections::HashSet<String>) {
    use ast::ExprKind as K;
    match &e.kind {
        // A shim's bindings are plain names; only its body holds expressions.
        K::Shim(_, _, body) => collect_operators(body, out),
        // An operator use is a call named for the spelling written, so the
        // spellings are read straight off the callees. `operator_named_forms`
        // answers "is this an operator" without the `is_operator_name` hook,
        // which this (loader-independent) tooling has no reason to install.
        K::Call(name, args, _) => {
            if !operator_named_forms(name.name()).is_empty() {
                out.insert(name.name().to_string());
            }
            for a in args {
                collect_operators(a, out);
            }
        }
        K::Neg(x) => {
            out.insert("-".to_string());
            collect_operators(x, out);
        }
        K::Field(x, _)
        | K::Try(x)
        | K::Return(x)
        | K::KwArg(_, x, _)
        | K::TypeArgs(x, _)
        | K::Spread(x) => collect_operators(x, out),
        // An `Assign` LHS is a place (idents/fields only), so it can't
        // contain an operator — only the value and body need walking.
        K::Seq(a, b)
        | K::Index(a, b)
        | K::Let(_, _, a, b)
        | K::LetMut(_, _, a, b)
        | K::Assign(_, a, b)
        | K::For(_, _, a, b)
        | K::While(a, b) => {
            collect_operators(a, out);
            collect_operators(b, out);
        }
        K::If(a, b, c) => {
            collect_operators(a, out);
            collect_operators(b, out);
            collect_operators(c, out);
        }
        K::Slice(a, b, c) => {
            collect_operators(a, out);
            collect_operators(b, out);
            if let Some(c) = c {
                collect_operators(c, out);
            }
        }
        K::ArrayLit(args) | K::SetLit(args, _) => {
            for a in args {
                collect_operators(a, out);
            }
        }
        K::DictLit(pairs) => {
            for (k, v) in pairs {
                collect_operators(k, out);
                collect_operators(v, out);
            }
        }
        K::Construct(_, inits) => {
            for i in inits {
                collect_operators(&i.value, out);
            }
        }
        K::Match(s, arms) => {
            collect_operators(s, out);
            for a in arms {
                collect_operators(&a.body, out);
            }
        }
        K::IfLet(arm, s, else_b) => {
            collect_operators(s, out);
            collect_operators(&arm.body, out);
            collect_operators(else_b, out);
        }
        K::Lambda(_, body) => collect_operators(body, out),
        K::TupleLit(elems) => {
            for e in elems {
                collect_operators(e, out);
            }
        }
        K::Num(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::None | K::Unit | K::Ident(_) => {}
    }
}

/// The builtin error type. For now it's represented exactly like `str` (the
/// 24-byte value in `aipl-codegen/src/str24.rs` — inline, a refcounted buffer, or
/// a rope, with no NUL terminator in any of them) and behaves like
/// one everywhere — but it's a *distinct* named type so error-specific
/// functionality can be hung on it later. It's the Err payload of every
/// error-returning builtin (e.g. the file functions' `str!Error` / `!Error`).
pub const ERROR: &str = "Error";

pub fn error_ty() -> Type {
    Type::Named(ERROR.into())
}

pub fn is_error(t: &Type) -> bool {
    matches!(t, Type::Named(s) if s == ERROR)
}

/// Every builtin's signature, written as AIPL source. These are *declarations*
/// only — the checker (`aipl-codegen`) resolves a call to `map`/`value_or`/
/// `print`/… against them exactly as it would a user function, with no notion
/// that they're builtin; monomorphization (`aipl-mono`) substitutes the same
/// declared signatures to infer a builtin call's concrete return type during
/// its own inference pass. Each body is a trivial value of the declared return
/// type so it type-checks like any function — it is never compiled
/// (monomorphization and codegen lower the real implementations).
///
/// It also carries builtin *type* declarations (e.g. `Span`, `ExecResult`) for the
/// same reason: the checker recognizes them as ordinary structs with no notion
/// that they're builtin, while `aipl-codegen` separately seeds its own struct
/// layout table with them (see `builtin_struct_decls`/`build_struct_layouts`),
/// since a user program's own compiled AST never contains these items.
///
/// Authoring notes: `<T: any>` (unconstrained) and `<T: ord>` (comparable
/// scalars — integers and `char`, see [`ast::Bound`]) are the only valid
/// generic bounds; effects precede
/// the return type (`!read_files -> str!Error`); a `mut self` first parameter
/// marks a mutating method. First parameters are named `self` so the
/// receiver-style builtins are method-callable (`xs.map(..)`, `opt.value_or(..)`).
pub const BUILTIN_SIGNATURES: &str = r#"
// A half-open byte range `[start, end)`, e.g. a source-text location.
struct __builtin_Span { start: u64, end: u64 }
// A finished child process's captured output and exit status.
struct __builtin_ExecResult { stdout: str, stderr: str, exit_code: i64 }
// What one `measure(..)` call cost: heap allocations made, bytes those requested,
// the most heap that was live at once (counted from what was live when the
// window opened), and CLIF instructions executed. All deterministic for a given
// program and compiler.
struct __builtin_Measurement { allocations: u64, bytes_allocated: u64, peak_live_bytes: u64, instructions: u64 }
// Where a `shim random { .. }` block's numbers come from: the seed to start the
// sequence at, or `none` to seed from the clock. `seeded_with_int` and
// `seeded_with_clock` make one.
struct __builtin_RandomProvider { seed: u64? }
// A rectangular grid of `T`, written `T[,]`: `width` cells to a row, the rows
// stored one after another in `cells`.
struct __builtin_Grid<T: any> { width: u64, cells: T[] }

# Write `self` to standard output, followed by a newline.
fn __builtin_print(self: str) !prints {}
# Write `self` to standard output with nothing after it, and have it appear at
# once: the next `put` or `print` carries on from where this one stopped. With
# `move_to` it redraws part of the screen in place.
fn __builtin_put(self: str) !prints {}
# Split on each occurrence of `sep`, returning the parts (slices/views of `self`).
fn __builtin_split(self: str, sep: str) -> str[] { [] }
# `self.split(sep).len()` without building the parts: how many there would be,
# which is one more than the occurrences of `sep` (an empty `sep` never
# matches, so it gives 1). Written by the compiler's operation-fusion pass for
# that chain — `src.lines().len()` is a newline count with no array behind it;
# also callable directly.
fn __builtin_split_len(self: str, sep: str) -> u64 { 0 }
# Concatenate the parts with `sep` between consecutive elements.
# Flatten a sequence of sequences, placing a separator between consecutive
# parts: `[[1,2],[3]].join(sep=0)` is `[1,2,0,3]`. Generic in the *element*
# type, so the receiver is one level deeper than it looks — for `T = char` it
# reads `str[] -> str` with a `char*` (i.e. `str`) separator, which is the
# string join. Every separator is variadic (a sequence, one element, or an
# optional one); `sep` defaults to empty, so `xs.join()` concatenates.
#
# `final_sep` goes in the last gap and `only_sep` in the sole gap of a two-part
# join — the same gap seen two ways, since a list of two has no "rest" for `sep`
# to appear in. Both default to `sep`, so an ordinary join is unaffected; naming
# them is what writes an English list in one call:
# `xs.join(sep=", ", final_sep=", or ", only_sep=" or ")` gives `"a"`,
# `"a or b"`, `"a, b, or c"`.
fn __builtin_join<T: any>(self: T[][], sep: T* = [], final_sep: T* = sep, only_sep: T* = sep) -> T[] { [] }

# The file builtins return a Result; the `ok(..)` body coerces to the declared
# `..!Error` (codegen builds the real ok/err).
fn __builtin_read_file_to_string(self: str) !read_files -> str!Error { ok("") }
# Write `contents` to the file at `self`, replacing what was there. `err(..)` if it cannot be written.
fn __builtin_write_string_to_file(self: str, contents: str) !write_files -> !Error { ok() }
# Every file at or below the directory `self`, recursively: each element is the
# path `self` joined with the entry's path beneath it (`"dir/sub/f.txt"`), in
# unspecified order. Directories are descended into, not listed; a symlink is a
# file (never followed). `err(..)` if the tree can't be walked at all — an
# unreadable directory, a non-UTF-8 name, or an entry whose kind the filesystem
# won't report.
fn __builtin_list_files(self: str) !list_files -> str[]!Error { ok([]) }
# The next line of standard input, without its line ending (`\n`, or `\r\n`):
# `some(line)` for each line, `none` once input is exhausted. A final line with
# no newline after it is still a line, and an empty line is `some("")`, so
# `none` always means end of input. Standard input is read in order across
# calls, and is not echoed or prompted for: print the prompt first.
fn __builtin_read_line() !read_stdin -> str? { none }
# The next byte of standard input, as soon as it is typed: `some(byte)` for each
# byte, `none` once input is exhausted. While the call waits, a terminal on
# standard input is taken out of line mode and stops echoing, so a key arrives
# without Enter (an arrow key is the three bytes `27 91 65`..`68`); the terminal's
# settings are put back before the call returns. Input that is not a terminal is
# read as it is. Bytes come from the same stream as `read_line`, in order.
fn __builtin_read_byte() !read_stdin_raw -> u8? { none }
# Erase everything on the terminal and put its cursor at the top left. The terminal
# is told by the runtime, in the control sequence it understands: no `str` or
# `char` can hold one, so this and `move_to` are the only ways to send it. Output
# that is not a terminal receives the sequence as text.
fn __builtin_clear_terminal() !terminal {}
# Put the terminal's cursor at `row` and `column`, counting from 0 at the top left;
# what is printed next starts there. A position past the terminal's edge is
# answered however the terminal answers it.
fn __builtin_move_to(row: u64, column: u64) !terminal {}
# Switch the terminal to a second, blank screen to draw on. What was on the screen
# before, and where its cursor was, comes back at `leave_alternate_screen`, so
# nothing drawn in between is left behind. A program that enters should leave
# before it ends. Terminals that have no second screen draw on the first.
fn __builtin_enter_alternate_screen() !terminal {}
# Switch the terminal back to the screen that `enter_alternate_screen` left, with
# its contents and cursor as they were.
fn __builtin_leave_alternate_screen() !terminal {}
# What the cell `id` holds, which is where a shim closure keeps a variable it
# captured: a shim stands in for an operation at every call site below it, so the
# closure it installs is a plain function with nowhere to carry what it captured.
# The result has the type the lowering of the closure shim (`aipl-mono`) recorded
# for the cell. Only that lowering writes it.
fn __builtin_shim_cell_get(id: u64) -> u64 { 0 }
# Put `value` in the cell `id`, which takes a reference of its own to it and lets
# go of what it held. The other half of `shim_cell_get`.
fn __builtin_shim_cell_set<R: any>(id: u64, value: R) {}
# The next random number, uniform over all of `u64`. `!random` is the one effect
# with no default: there is no generator unless a `shim random` installs one, so
# `main` cannot declare it and a program that uses it must say where its
# randomness comes from — `shim random seeded_with_clock() { .. }` for a fresh
# sequence each run, `shim random seeded_with_int(n) { .. }` for a repeatable one.
fn __builtin_random_u64() !random -> u64 { 0 }

# Wall-clock nanoseconds since the Unix epoch (1970-01-01 UTC). Unsigned and
# 64-bit, so it counts up to the year 2554; a clock set before the epoch reads
# as 0. This is the system's *wall* clock, which can jump backwards (NTP,
# manual changes) — successive readings are not guaranteed to increase, so it
# measures "when", not reliably "how long".
fn __builtin_now_nanos() !clock -> u64 { 0 }
# Nanoseconds from the system's monotonic clock: it only ever counts up (no NTP
# step or manual clock change moves it), so a *difference* between two readings
# is a real elapsed duration. Its origin is unspecified — an absolute reading
# means nothing on its own, and nothing across processes or machines. Use this
# to measure "how long"; use `now_nanos` for "when".
fn __builtin_monotonic_now() !clock -> u64 { 0 }
# Spawn `self` with `args` (no shell involved) and wait for it to finish:
# `ok(ExecResult)` whenever it was actually launched, whatever it then exited
# with; `err(message)` only if it couldn't be launched at all (not found,
# permission denied, ...). `args` is a keyword parameter defaulting to no
# arguments, so `"true".execute_program()` runs a bare program and
# `"echo".execute_program(args = ["hi"])` passes argv.
fn __builtin_execute_program(self: str, args: str[] = []) !execute_program -> __builtin_ExecResult!Error {
    ok(__builtin_ExecResult { stdout: "", stderr: "", exit_code: 0 })
}

# The text of a value, exactly what `` `{v}` `` interpolates: a `char` or `str` renders bare,
# and one nested in an array, struct or optional keeps its literal form.
fn __builtin_to_str<T: any>(self: T) -> str { "" }
# `self.to_str().len()` without building the string: how long the rendering
# would be, measured rather than materialized. `to_str` already works in two
# passes — measure the total length, allocate exactly that, then write — so
# this is the first pass alone: no buffer, no copy, no refcount. Written by the
# compiler's operation-fusion pass for that chain; also callable directly.
fn __builtin_to_str_len<T: any>(self: T) -> u64 { 0 }
# Structural hash, consistent with `==`.
fn __builtin_hash<T: any>(self: T) -> i64 { 0 }
# `self` without the whitespace at either end.
fn __builtin_trim(self: str) -> str { self }
# The content of `self` in one *named* runtime representation, instead of
# whichever one it arrived in. What they are for, and why naming the result beats
# the operation that happens to produce it, is DESIGN_PRINCIPLES.md's "Just say
# what you want"; the representations themselves are described at the top of
# `crates/aipl-codegen/src/str24.rs`.
#
# A fresh refcounted buffer, at every length — so it allocates even where the
# content would have fit inline, which is the request.
fn __builtin_heap_str(self: str) -> str { self }
# A rope node over the two, built even when an operand is empty (where `+++`
# hands the other side back instead, and so cannot promise a rope).
fn __builtin_rope_str(self: str, other: str) -> str { self }
# Internal: `inline_str`'s unchecked half. Content past the inline capacity has
# no inline form and comes back untouched, so `inline_str` does the length check
# and answers `none` rather than calling this with content that cannot fit.
fn __builtin_pack_inline(self: str) -> str { self }
# Internal: the bound `inline_str` checks against, so nothing hardcodes it.
fn __builtin_inline_capacity() -> u64 { 0 }
# Internal: the counting window behind `measure`. `!measure` is an effect only so
# that nothing outside a `.performance` block can call these (or `measure`), and
# so that the optimizer, which never reorders or drops an effectful call, keeps
# the window around exactly the work it was opened for.
fn __builtin_measure_begin() !measure {}
# Release `self` now, inside the window. The value being measured has to be
# *used* by something the optimizer cannot remove, or a pure call whose result
# nobody reads is dropped altogether; and its drop is part of what the work
# costs, so it happens before the window closes rather than at scope exit.
fn __builtin_measure_consume<R: any>(self: R) !measure {}
# Close the counting window opened by `measure_begin`. Internal to `measure`.
fn __builtin_measure_end() !measure {}
# One of the counts of the window just closed. Internal to `measure`.
fn __builtin_measure_read(which: u64) !measure -> u64 { 0 }
# True if the character is whitespace: a space, tab or line break.
fn __builtin_is_whitespace(self: char) -> bool { false }
# True if `self` begins / ends with the pattern — `str` bytes or `T[]`
# elements. The pattern is variadic: the sequence (a `str` for a `str`
# receiver), one element, or an optional element; the empty pattern — `""`,
# `[]`, `none` — always matches. Pass a single `char` when that is what is
# being asked (`s.starts_with('#')`, not a one-char str): the char shape is one
# byte compare in the runtime, where a one-char `str` is a window compare. A str
# receiver is dispatched in the checker / codegen (the `T[]` signature doesn't
# unify with `str`).
fn __builtin_starts_with<T: any>(self: T[], prefix: T*) -> bool { false }
# True if `self` ends with `suffix`. The pattern is variadic as `starts_with`'s is.
fn __builtin_ends_with<T: any>(self: T[], suffix: T*) -> bool { false }
# `self[at..].starts_with(prefix)` without building the slice: the same answer,
# comparing in place from `at`. `at` clamps like a slice bound, so an `at` past
# the end matches only the empty pattern. The pattern is variadic exactly as
# `starts_with`'s, and dispatched with it in the checker / codegen. Written by
# the compiler's operation-fusion pass; also callable directly.
fn __builtin_starts_with_at<T: any>(self: T[], prefix: T*, at: u64) -> bool { false }
# True if `self` contains the needle: a `T[]` (or `str`) needle matches as a
# contiguous subsequence (substring), a `T` (or `char`) as a single element,
# and a `T?` as its element when `some` — a `none` needle is nothing to find,
# so it's `false` (unlike `starts_with`/`ends_with`, whose `none` pattern is
# the empty pattern and matches). A str receiver is dispatched in the
# checker / codegen (the `T[]` signature doesn't unify with `str`).
fn __builtin_contains<T: any>(self: T[], needle: T[]) -> bool { false }
# Smaller / larger of two comparable values (codegen compares and selects).
# `ord` is the same bound `minimum`/`maximum` carry — integers, `char` and
# `str` — so these work at any integer width rather than `i64` alone.
fn __builtin_min<T: ord>(self: T, other: T) -> T { self }
# The larger of two comparable values. See `min`.
fn __builtin_max<T: ord>(self: T, other: T) -> T { self }
// `minimum` and `maximum` are not declared here: they are reductions, and a
// reduction's signature follows from its step's (`min`, `max`, just above) —
// `<T: ord>(self: T[]) -> T?` from `<T: ord>(self: T, other: T) -> T`. See
// `Callee::reduction_step` and `with_reduction_signatures`, which adds them.
# Element/byte count. Unsigned: a length is never negative, so `u64` keeps
# `len`-derived arithmetic (`a.len() - b.len()`, capacity math) in the
# saturating-at-zero unsigned world rather than silently going negative.
# Index/slice bounds accept either signedness, so `xs[xs.len() - 1]` still
# works without a conversion.
fn __builtin_len<T: any>(self: T[]) -> u64 { 0 }
# Whether the receiver holds anything at all — the same question
# `len(self) != 0` asks, without routing an answer that is already a `bool`
# through a count and a comparison. Takes every receiver `len` does (array,
# `str`, set, dict), dispatched the same way in the checker and codegen; the
# `len_gt_zero` lint points every `0 < x.len()` here.
fn __builtin_is_nonempty<T: any>(self: T[]) -> bool { false }
# The other half of the same question. Written as its own builtin rather than
# left to `!x.is_nonempty()` or `x.len() == 0` because those say what the test
# *isn't*, or make the reader recognize an idiom; the `is_empty` lint is what
# keeps the corpus on this spelling.
fn __builtin_is_empty<T: any>(self: T[]) -> bool { false }
# True if the optional holds a value.
fn __builtin_is_some<T: any>(self: T?) -> bool { false }
# Character classification: ASCII whitespace (space/tab/newline/carriage return).
fn __builtin_is_space(self: char) -> bool { false }
# Character classification: ASCII decimal digit ('0' through '9').
fn __builtin_is_digit(self: char) -> bool { false }
# ASCII decimal digit ('0'..'9') to its 0..9 value, `none` for any other char.
fn __builtin_to_digit(self: char) -> i64? { none }
# An integer as another integer type, `none` when its value does not fit. The
# target is the use site's: `let b: u8? = n.narrow();` asks for a `u8`, and
# the checker locks that expected `u8?` onto the call (see `needs_lock`), as
# it does a `to_set`'s order. With no integer optional expected there is no
# target, which is an error. The one way to an integer type that cannot hold
# every value of the source: a lossless widening needs no call at all.
fn __builtin_narrow<T: any>(self: T) -> T? { none }

# Set ops: membership and union. `#{T}` in a signature takes a set of any
# order, and a set *returned* takes the receiver's (see `inherit_set_order`).
fn __builtin_has<T: any>(self: #{T}, x: T) -> bool { false }
# The elements of either set.
fn __builtin_union<T: any>(self: #{T}, other: #{T}) -> #{T} { self }
# The elements of an *ordered* set, in its order, as an array — the checker
# refuses a `#{T}` receiver, whose order is not one to write code against.
# Free at runtime: a set is an array block, so this is the same block retained.
fn __builtin_to_array<T: any>(self: #{T}) -> T[] { [] }
# An array's distinct elements as a set. Which set — `#{T}`, `#>{T}` or
# `#<{T}` — comes from the use site, like the type of a bare `#{}`: the
# checker types the call `SetOrder::Context` and locks the expected type onto
# it, and codegen builds the set in that order (unordered with no context).
fn __builtin_to_set<T: any>(self: T[]) -> #{T} { #{} }

# Dict ops: lookup (none if absent), membership, and removal.
fn __builtin_get<K: any, V: any>(self: #{K: V}, key: K) -> V? { none }
# True if a value is stored under `key`.
fn __builtin_contains_key<K: any, V: any>(self: #{K: V}, key: K) -> bool { false }
# Drop `key`'s entry, in place. A key the dict does not hold is not an error —
# the point of the call is that the key is gone afterwards, and it already is —
# so this is the removal counterpart of `push`, not of an indexing read.
#
# The surviving entries keep the order they were inserted in, so iterating a
# dict after a removal hands back what it did before, minus one. A `mut self`
# receiver, so `set d.remove_key(k);` writes back the way every mutating method
# does; in expression position it copies-and-modifies like one too.
fn __builtin_remove_key<K: any, V: any>(mut self: #{K: V}, key: K) {}
# Bind `key` to `value`, in place: a key the dict already holds takes the new
# value and keeps its place, any other is added after the existing ones — so
# the last write to a key wins. The counterpart of `remove_key`, with the same
# `mut self` receiver. Internal: `to_dict` is the spelling.
fn __builtin_insert_key<K: any, V: any>(mut self: #{K: V}, key: K, value: V) {}

# `map` also takes an optional receiver — `opt.map(f)` is `U?`, `some(f(v))`
# for `some(v)` and `none` for `none` — which the checker and mono dispatch
# on the receiver's type; this signature is the array form the declaration
# language can spell.
fn __builtin_map<T: any, U: any>(self: T[], f: (T) -> U) -> U[] { [] }
# The elements for which `pred` is true, in order.
fn __builtin_filter<T: any>(self: T[], pred: (T) -> bool) -> T[] { self }
# `self.filter(keep).map(f)` in one pass over the elements, instead of one pass
# each with an array between them. Written by the compiler's operation-fusion
# pass; also callable directly, and worth writing by hand when the two halves
# belong together.
fn __builtin_filter_map<T: any, U: any>(self: T[], keep: (T) -> bool, f: (T) -> U) -> U[] { [] }
fn __builtin_intersperse<T: any>(self: T[], sep: T) -> T[] { self }
# True if both values are the same case of the variant, whatever their payloads.
fn __builtin_same_case<T: variant>(self: T, other: T) -> bool { false }
# The name of the case `self` was built with, without its payload:
# `Name("x").case_name()` is `"Name"`, and a payload-free `Space` is
# `"Space"`. The tag names the case, so this reads no payload — which is
# what lets it be generic over every variant, like `same_case`.
fn __builtin_case_name<T: variant>(self: T) -> str { "" }
# `v.case_of()` — which case `v` is, as a `Case<T>` carrying no payload. The
# counterpart to naming a case directly (`Term(Str)`): this is how a *value*
# is compared against one, since `==` on two `Case<T>`s is a tag compare.
# `same_case(a, b)` remains the way to compare two values.
fn __builtin_case_of<T: variant>(self: T) -> Case<T> { __builtin_case_of(self) }
fn __builtin_count_is_less_than<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_at_most<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_greater_than<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_at_least<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_equal<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_not_equal<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_first<T: any>(self: T[]) -> T? { none }
fn __builtin_last<T: any>(self: T[]) -> T? { none }
# The refinement `T[] without []` granted without a test — see
# `Callee::AssumeNonempty`. A signature only: the checker types the call, and
# `erase_refinements` then replaces it with its argument, which is what it is
# at runtime.
fn __builtin_assume_nonempty<T: any>(self: T[]) -> T[] without [] { __builtin_assume_nonempty(self) }
fn __builtin_drop_first<T: any>(self: T[]) -> T[] { self }
fn __builtin_drop_last<T: any>(self: T[]) -> T[] { self }
fn __builtin_drop_n<T: any>(self: T[], n: u64) -> T[] { self }
fn __builtin_drop_last_n<T: any>(self: T[], n: u64) -> T[] { self }
# A copy of `self` with the element at `index` replaced by `value`, or `none` when
# `index` is past the end: there is no element to replace, and `self[index]` is
# `none` there too. Arrays have no element assignment because a write past the end
# has nowhere to go; this is the version that answers. The receiver is unchanged
# as far as anything else can tell, but when `self` has no other owner — a fresh
# array, or a binding at its last use — it is written in place and handed back,
# so a chain of updates allocates nothing. `set xs = xs.with(i, v).value_or(xs)`
# keeps `xs` alive for the fallback, and so copies; write `xs.with(i, v)` where
# `xs` is not read again to avoid the copy. Not for a `str`.
fn __builtin_with<T: any>(self: T[], index: u64, value: T) -> T[]? { none }
# Replace the element at `index` with `value`, in place: `set xs.try_set(i, v);`.
# When `index` is past the end there is no element to replace and `xs` is left as
# it was — a write that misses does nothing, as removing a key that is not there
# does nothing. Reach for `with` when the miss should be told apart. Like `push`
# it is a mutating method, so `xs.try_set(i, v)` as an expression is the updated
# copy and leaves `xs` alone. Not for a `str`.
fn __builtin_try_set<T: any>(mut self: T[], index: u64, value: T) {}
// NOTE: `all`, `grid_of`, `words`, `cartesian_product`, `count_while`, `count_if`, `count_if_is_less_than` and its five siblings, `find_if`, `collect_arms`, `as_case`, `collect_ok`, `find_index`, `find_map`, `map_find_if`, `map_join`,
// `join_is_nonempty`, `join_is_empty`, `reduce`, `repeat`, `map_reduce`, `filter_map_reduce`, `split_map`, `reverse_find_map`, `nonempty_first`, `nonempty_last`, `ensure_nonempty`,
// `is_all_whitespace`, `is_some_and`, `int_parse`, `trim_while`, `try_map`,
// `extend_optional`, `inline_str`, `measure`, `range`, `set_map`, `to_dict`, `map_to_dict`, `tuple_windows`, `union_all`,
// `value_or`, and `value_or_err` are
// *not* declared here — they're implemented in AIPL (`aipl-mono/src/builtin_*.aipl`),
// which is the single source of both their body and their signature: the loader
// loads each as a file of any program that reaches it. See `aipl_builtins`.
# `f` applied to the elements of both arrays pairwise, stopping at the shorter one.
fn __builtin_zip_with<T: any, U: any, V: any>(self: T[], other: U[], f: (T, U) -> V) -> V[] { [] }
# Add `x` at the end of `self`. `set xs.push(x);` appends in place; as an expression it is the extended copy.
fn __builtin_push<T: any>(mut self: T[], x: T) {}
# Append `other` to `self` — `push` for a whole array, and the way to grow a
# `str` in place (`set s.extend(t)`; a `str` is its `char` sequence, so it takes
# a `str` or a `char[]` source). A builtin rather than an AIPL loop over `push`
# so it can size the destination once: growing by `other.len()` in a single
# reserve turns N reallocations into at most one, and the elements move as one
# `memcpy` plus one retain pass. The `extend_longhand` lint sends the rebuild
# spelling, `set s = s +++ t;`, here.
#
# The source is variadic in shape, like `starts_with`'s pattern: the sequence,
# one element, or an *optional* element — `set xs.extend(maybe)` appends what
# `maybe` holds and leaves `xs` alone when it holds nothing, which is what a
# hand-rolled `push_optional` helper used to be for. Mono resolves the shape at
# the call site (see its `Extend` arm): one element is exactly `push`, and an
# optional becomes the sequence it stands for, so codegen keeps a single
# `extend`.
fn __builtin_extend<T: any>(mut self: T[], other: T*) {}
# Make room in `self` for `additional` more elements (bytes, for a `str`)
# without changing its contents, so that many appends fit in one allocation
# — `set out.reserve(n)` ahead of a loop of `push`/`extend`, where the loop
# knows how much it will add and the appends would otherwise grow the buffer
# by doubling. Exact rather than doubled: the caller has said what it needs. A
# `str` receiver keeps its representation (an inline value that still fits
# stays inline).
fn __builtin_reserve<T: any>(mut self: T[], additional: u64) {}
# Reverse the elements of an array or the bytes of a string.
fn __builtin_reverse<T: any>(self: T[]) -> T[] { [] }
# Ascending sort. `ord` restricts `T` to comparable elements (integer, char, or
# str), enforced generically by the checker's bound-checking.
fn __builtin_sort<T: ord>(self: T[]) -> T[] { [] }
fn some<T: any>(x: T) -> T? { none }

// Test-runner hooks. `__assert(cond, loc)` is what `assert(cond)` lowers to
// inside a `.test({ .. })` body; the other three are called by the synthesized
// `__test_main` driver (see `build_test_program`). All are effect-free so test
// code needs no effect annotations to call them.
fn __assert(cond: bool, loc: str) {}
fn __test_begin(name: str) {}
fn __test_end() {}
fn __test_summary() -> i64 { 0 }
// The debugging hook. `trace(expr)` lowers to `__trace(msg)` (the loader's
// `bake_traces`), where `msg` already holds the expression's location, its own
// source text and its rendered value. Shares `print`'s runtime symbol but
// declares **no** `!prints` effect, which is the whole point: a trace can go
// anywhere without changing a signature. `aipl check` is what keeps that from
// being a hole — it fails, after running the tests, on a program that still
// contains one (`trace_diagnostics`).
fn __trace(self: str) {}
// Internal: emitted by the compiler for array-literal spreads (`[..xs, y]`).
// `reserve` sizes the accumulator for the whole literal up front and makes it
// uniquely owned; `append`/`concat` then write into that reserved capacity in
// place. Named `__aipl_*` (not `__builtin_*`) so no import can name them.
fn __aipl_arr_reserve<T: any>(self: T[], extra: u64) -> T[] { self }
fn __aipl_arr_append<T: any>(self: T[], x: T) -> T[] { self }
fn __aipl_arr_concat<T: any>(self: T[], other: T[]) -> T[] { self }
// Internal: emitted by the compiler for template-literal concatenation.
fn __aipl_concat(a: str, b: str) -> str { "" }
// Internal: what a `shim random <provider>(..) { .. }` lowers to. The provider
// installs `__random_next` as the implementation of `random_u64`, and the first
// thing in the body seeds the generator it reads: `__random_seed` with a given
// seed, `__random_seed_from_clock` with the (shimmable) clock. The generator's one
// word of state is saved and restored with the shim, so a shim nests.
// Internal: what `try_set` lowers to, and what `xs.with(i, v).value_or(xs)` fuses
// into. Reads `xs` once.
fn __with_or_self<T: any>(self: T[], index: u64, value: T) -> T[] { self }
fn __random_next() -> u64 { 0 }
fn __random_seed(seed: u64) {}
fn __random_seed_from_clock() !clock {}
// Internal: emitted for each interpolation in a template literal.
// Passes a `str` through unchanged, widens a `char` to the one-char `str`
// holding it, and converts any other type via `to_str`.
fn __template_interp<T: any>(self: T) -> str { "" }
"#;

/// `items` — the parsed [`BUILTIN_SIGNATURES`] — with a declaration added for every
/// reduction, synthesized from the signature of its step.
///
/// A reduction ([`Callee::reduction_step`]) takes a sequence and combines its
/// elements with a binary step, so its signature is the step's with the types
/// lifted: a step `<T: ord>(self: T, other: T) -> T` makes the reduction
/// `<T: ord>(self: T[]) -> T?`. Nothing in the reduction's own declaration says
/// anything the step's does not, so it is not written down — which is also what
/// stops the two from disagreeing about a bound or an effect.
///
/// The step has to be declared (here or by the caller, in `items`) with the shape
/// `(A, A) -> A`; a reduction naming one that is not is a bug in the table, and
/// says so rather than inventing a signature.
pub fn with_reduction_signatures(mut items: Vec<ast::Item>) -> Vec<ast::Item> {
    let mut synthesized = Vec::new();
    for reduction in builtin::Builtin::all().iter().map(|b| &b.callee) {
        let Some(step) = reduction.reduction_step() else {
            continue;
        };
        let decl = items
            .iter()
            .find_map(|item| match item {
                ast::Item::Fn(f) if f.name.text == step.name() => Some(f),
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!(
                    "{reduction} is a reduction by {step}, which has no declaration to take its signature from"
                )
            });
        let element = match decl.sig.params.as_slice() {
            [first, second]
                if first.ty == second.ty && decl.sig.return_ty.as_ref() == Some(&first.ty) =>
            {
                first.ty.clone()
            }
            _ => panic!("{reduction}'s step {step} must have the shape (A, A) -> A"),
        };
        synthesized.push(ast::Item::Fn(ast::Function {
            name: SpanStr::synthetic(reduction.name()),
            is_pub: true,
            sig: ast::Signature {
                type_vars: decl.sig.type_vars.clone(),
                params: vec![ast::Param {
                    name: "self".to_string(),
                    ty: ast::Type::Array(Box::new(element.clone())),
                    mutable: false,
                    arity: ast::Arity::One,
                    default: None,
                    implicit_some: false,
                }],
                effects: decl.sig.effects.clone(),
                return_ty: Some(ast::Type::Optional(Box::new(element))),
            },
            // A declaration only: the checker reads the signature, and the body is
            // never run. `none` is a well-typed body for an optional result.
            body: ast::Expr::new(ast::ExprKind::None, 0..0),
            test_body: None,
            test_fns: Vec::new(),
            doc: None,
        }));
    }
    items.extend(synthesized);
    items
}

/// The *concatenated-string* representation of `str`: an internal, mono-only
/// pseudo-type that flows out of `a + b` (string concat) to mark a value built as
/// a lazy concat node (see `aipl_concat_lazy`). To the source author it is just a
/// `str` — it never appears in source and the standalone checker never sees it.
/// Its only role is in monomorphization: passing a concat-typed value to a
/// `fn(s: str)` selects a distinct, concat-specialized instance of that function
/// (the `$c{i}` instances), mirroring how `str_params`/`owned_params` specialize.
/// It has the `str` runtime representation (`is_str_repr` below), so all codegen
/// machinery treats it exactly like a `str`.
pub fn concat_str_ty() -> Type {
    Type::ConcatStr
}

pub fn is_concat_str(t: &Type) -> bool {
    matches!(t, Type::ConcatStr)
}

/// Whether `t` has the `str` runtime representation: `str` itself, the builtin
/// `Error` type (currently a string under the hood), or the internal concat-str
/// representation. These share all codegen machinery — refcounting, equality,
/// hashing, rendering.
/// The [`ast::ConcreteType`] counterparts of the shared type predicates below.
///
/// Deliberately the *same names* in their own module, so a consumer that works
/// post-monomorphization switches representation by changing its `use` line and
/// nothing else. The bodies mirror their abstract twins arm for arm; where one
/// of those has a case for an abstract variant, the concrete version simply has
/// no such case to write.
pub mod concrete {
    use super::ast::{ConcreteType, Primitive};
    use super::ERROR;

    pub fn is_unit(t: &ConcreteType) -> bool {
        matches!(t, ConcreteType::Unit)
    }

    pub fn is_error(t: &ConcreteType) -> bool {
        matches!(t, ConcreteType::Named(s) if s == ERROR)
    }

    pub fn error_ty() -> ConcreteType {
        ConcreteType::Named(ERROR.into())
    }

    pub fn is_concat_str(t: &ConcreteType) -> bool {
        matches!(t, ConcreteType::ConcatStr)
    }

    pub fn is_none_inner(t: &ConcreteType) -> bool {
        matches!(t, ConcreteType::NoneInner)
    }

    pub fn is_int_ty(t: &ConcreteType) -> bool {
        matches!(t, ConcreteType::Primitive(p) if p.is_int())
    }

    pub fn is_str_repr(t: &ConcreteType) -> bool {
        matches!(t, ConcreteType::Primitive(Primitive::Str)) || is_error(t) || is_concat_str(t)
    }

    pub fn is_set_elem(t: &ConcreteType) -> bool {
        is_int_ty(t)
            || matches!(
                t,
                ConcreteType::Primitive(Primitive::Bool | Primitive::Char | Primitive::Str)
            )
            // A `Case<V>` is its tag — one word, compared as an integer.
            || matches!(t, ConcreteType::Case(_))
    }

    pub fn is_dict_key(t: &ConcreteType) -> bool {
        is_set_elem(t)
    }

    pub fn is_array_elem(t: &ConcreteType) -> bool {
        is_int_ty(t)
            || matches!(
                t,
                ConcreteType::Primitive(Primitive::Bool | Primitive::Char | Primitive::Str)
                    | ConcreteType::Array(_)
                    | ConcreteType::Set(..)
                    | ConcreteType::Dict(_, _)
            )
    }

    /// [`super::flex_int_ty`] over the post-monomorphization representation: a
    /// bare integer literal takes the width of the type it is being fitted
    /// against. Widens, delegates, and narrows back — the fitting rule reads
    /// the `Expr`, which is shared by both sides of monomorphization and so
    /// speaks the abstract type.
    pub fn flex_int_ty(
        e: &super::ast::Expr,
        ety: &ConcreteType,
        other: &ConcreteType,
    ) -> ConcreteType {
        super::flex_int_ty(e, &ety.widen(), &other.widen())
            .to_concrete()
            .unwrap_or_else(|| ety.clone())
    }

    pub fn type_name(t: &ConcreteType) -> String {
        match t {
            ConcreteType::Unit => "()".into(),
            ConcreteType::Primitive(p) => p.name().into(),
            ConcreteType::Named(s) => super::demangle_named(s),
            ConcreteType::Case(v) => format!("Case<{v}>"),
            ConcreteType::Optional(inner) => format!("{}?", type_name(inner)),
            ConcreteType::Array(inner) => format!("{}[]", type_name(inner)),
            ConcreteType::Set(inner, o) => format!("#{}{{{}}}", o.spelling(), type_name(inner)),
            ConcreteType::Dict(k, v) => format!("#{{{}: {}}}", type_name(k), type_name(v)),
            ConcreteType::Result(ok, err) => format!("{}!{}", type_name(ok), type_name(err)),
            ConcreteType::Fn(params, ret) => {
                let ps = params.iter().map(type_name).collect::<Vec<_>>().join(", ");
                format!("({ps}) -> {}", type_name(ret))
            }
            // Spelled exactly as the abstract `type_name` spells them, so a
            // diagnostic reads the same whichever side of mono produced it.
            ConcreteType::NoneInner => "__none__".into(),
            ConcreteType::EmptyArrayArg => "EmptyArray".into(),
            ConcreteType::NoneLiteralArg => "NoneLiteral".into(),
            ConcreteType::ConcatStr => "__concat_str__".into(),
        }
    }
}

pub fn is_str_repr(t: &Type) -> bool {
    matches!(t, Type::Primitive(Primitive::Str)) || is_error(t) || is_concat_str(t)
}

/// Strip the prefixes a name picks up on its way through the compiler: the
/// `__builtin_` of a builtin type and the `__m<N>__` of a per-file one.
pub fn strip_mangle_prefix(s: &str) -> &str {
    let s = s.strip_prefix("__builtin_").unwrap_or(s);
    if let Some(rest) = s.strip_prefix("__m") {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && rest[digits..].starts_with("__") {
            return &rest[digits + 2..];
        }
    }
    s
}

/// Render a (possibly mangled) named type for diagnostics. Strips the module /
/// `__builtin_` prefixes (see [`strip_mangle_prefix`]) and turns a generic
/// instance's mangled name back into source-like form: `Box$i64` → `Box<i64>`,
/// `Pair$i64$str` → `Pair<i64, str>`, a synthetic tuple `__tuple$i64$str` →
/// `(i64, str)`. An element that is itself an optional, array or instance was
/// flattened when mangled (`opt_`, `arr_`, and `_` for a nested `$`), so those
/// are read back as far as the flattening allows: `opt___builtin_Span` is
/// `Span?`. Every `type_name` renders a `Named` through this, so no diagnostic
/// shows a mangled name.
pub fn demangle_named(n: &str) -> String {
    let mut parts = n.split('$');
    let base = strip_mangle_prefix(parts.next().unwrap_or(n));
    let args: Vec<String> = parts.map(demangle_elem).collect();
    if args.is_empty() {
        base.to_string()
    } else if base == "__tuple" {
        format!("({})", args.join(", "))
    } else if base == "Grid" && args.len() == 1 {
        format!("{}[,]", args[0])
    } else {
        format!("{base}<{}>", args.join(", "))
    }
}

/// One mangled type argument back to source form — the `opt_`/`arr_`/`set_`
/// prefixes `mangle_type` writes, then the name they wrap.
fn demangle_elem(s: &str) -> String {
    if let Some(inner) = s.strip_prefix("opt_") {
        return format!("{}?", demangle_elem(inner));
    }
    if let Some(inner) = s.strip_prefix("arr_") {
        return format!("{}[]", demangle_elem(inner));
    }
    if let Some(inner) = s.strip_prefix("set_") {
        return format!("#{{{}}}", demangle_elem(inner));
    }
    strip_mangle_prefix(s).to_string()
}

/// A type's name as an *identity*: the mangled spelling, `Named` verbatim. This
/// is what an instance name is built from (mono's specialization memo keys on
/// it), where two per-file `Tok`s must stay two names. Everything shown to a
/// user goes through [`type_name`], which demangles.
pub fn raw_type_name(t: &Type) -> String {
    match t {
        Type::Unit => "()".into(),
        Type::Primitive(p) => p.name().into(),
        Type::Named(s) => s.clone(),
        Type::Case(v) => format!("Case<{}>", raw_type_name(v)),
        // A variable renders as the parameter the user wrote; the anonymous one
        // an `any` normalized to has no name to show, so it renders as `any`.
        Type::TypeVar(v) if v.is_empty() => "any".into(),
        Type::TypeVar(v) => v.clone(),
        // A type the checker could not pin down has no spelling of its own; `_`
        // is what a reader of a diagnostic should see in its place.
        Type::Unknown => "_".into(),
        Type::Optional(inner) => format!("{}?", suffix_operand(inner, raw_type_name)),
        Type::Array(inner) => format!("{}[]", suffix_operand(inner, raw_type_name)),
        Type::Set(inner, o) => format!("#{}{{{}}}", o.spelling(), raw_type_name(inner)),
        Type::Dict(k, v) => format!("#{{{}: {}}}", raw_type_name(k), raw_type_name(v)),
        Type::Result(ok, err) => format!(
            "{}!{}",
            suffix_operand(ok, raw_type_name),
            suffix_operand(err, raw_type_name)
        ),
        Type::Without(base, x) => format!("{} without {}", raw_type_name(base), x.spelling()),
        Type::Fn(params, ret) => {
            let ps = params.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("({ps}) -> {}", raw_type_name(ret))
        }
        Type::Tuple(elems) => {
            let es = elems.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("({es})")
        }
        Type::Generic(name, args) => {
            let as_ = args.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("{name}<{as_}>")
        }
        Type::Any => "any".into(),
        Type::NoneInner => "__none__".into(),
        Type::EmptyArrayArg => "EmptyArray".into(),
        Type::NoneLiteralArg => "NoneLiteral".into(),
        Type::ConcatStr => "__concat_str__".into(),
    }
}

/// A type as the operand of a postfix `?`/`[]`/`!E`: parenthesized when it is
/// a `without` refinement, which binds looser than every suffix — so the
/// rendering of `(T[] without [])?` reads back as the type it is, rather than as
/// `T[] without []?`, which the grammar refuses.
fn suffix_operand(t: &Type, render: fn(&Type) -> String) -> String {
    match t {
        Type::Without(..) => format!("({})", render(t)),
        _ => render(t),
    }
}

pub fn type_name(t: &Type) -> String {
    match t {
        Type::Unit => "()".into(),
        Type::Primitive(p) => p.name().into(),
        Type::Named(s) => demangle_named(s),
        Type::Case(v) => format!("Case<{}>", type_name(v)),
        // A variable renders as the parameter the user wrote; the anonymous one
        // an `any` normalized to has no name to show, so it renders as `any`.
        Type::TypeVar(v) if v.is_empty() => "any".into(),
        Type::TypeVar(v) => v.clone(),
        // A type the checker could not pin down has no spelling of its own; `_`
        // is what a reader of a diagnostic should see in its place.
        Type::Unknown => "_".into(),
        Type::Optional(inner) => format!("{}?", suffix_operand(inner, type_name)),
        Type::Array(inner) => format!("{}[]", suffix_operand(inner, type_name)),
        Type::Set(inner, o) => format!("#{}{{{}}}", o.spelling(), type_name(inner)),
        Type::Dict(k, v) => format!("#{{{}: {}}}", type_name(k), type_name(v)),
        Type::Result(ok, err) => format!(
            "{}!{}",
            suffix_operand(ok, type_name),
            suffix_operand(err, type_name)
        ),
        Type::Without(base, x) => format!("{} without {}", type_name(base), x.spelling()),
        Type::Fn(params, ret) => {
            let ps = params.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("({ps}) -> {}", type_name(ret))
        }
        Type::Tuple(elems) => {
            let es = elems.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("({es})")
        }
        Type::Generic(name, args) if args.len() == 1 && strip_mangle_prefix(name) == "Grid" => {
            format!("{}[,]", suffix_operand(&args[0], type_name))
        }
        Type::Generic(name, args) => {
            let as_ = args.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("{name}<{as_}>")
        }
        Type::Any => "any".into(),
        Type::NoneInner => "__none__".into(),
        Type::EmptyArrayArg => "EmptyArray".into(),
        Type::NoneLiteralArg => "NoneLiteral".into(),
        Type::ConcatStr => "__concat_str__".into(),
    }
}

/// Valid array element types: the 8-byte value types — every integer width
/// (`i8`..`i64`, `u8`..`u64`, each stored canonicalized in an 8-byte slot like
/// `i64`), `bool`, `char`, `str`, and the heap-block types, which are themselves
/// 8-byte pointers: (nested) arrays, sets and dicts. Structs and optionals are
/// inline composites wider than 8 bytes and aren't yet supported as elements.
pub fn is_array_elem(t: &Type) -> bool {
    is_int_ty(t)
        || matches!(
            t,
            Type::Primitive(Primitive::Bool | Primitive::Char | Primitive::Str)
                | Type::Array(_)
                | Type::Set(..)
                | Type::Dict(_, _)
        )
}

/// Valid set element types: the scalar value types — every integer width
/// (`i8`..`i64`, `u8`..`u64`), `bool`, `char`, and `str`. Scalars compare by
/// value; `str` compares by content (see the set runtime). A struct or variant
/// is also a set element, but needs the declarations to recognise, so the checker
/// and codegen ask for it themselves; nested containers (arrays/sets/optionals)
/// are not set elements.
///
/// Also the "storable scalar" test behind struct fields and variant payloads,
/// which hold exactly these inline.
pub fn is_set_elem(t: &Type) -> bool {
    is_int_ty(t)
        || matches!(
            t,
            Type::Primitive(Primitive::Bool | Primitive::Char | Primitive::Str)
        )
        // A `Case<V>` is its tag — one word, compared as an integer — so a set
        // of cases (`#{Case<Tok>}`, a FIRST set's kinds) is a set of integers.
        || matches!(t, Type::Case(_))
}

/// Valid dict *key* types: the same scalar/`str` types a set holds (keys are
/// compared/deduped exactly like set elements). Values, by contrast, may be any
/// value type a struct field can hold (scalars, `str`, arrays, optionals,
/// structs), validated separately.
pub fn is_dict_key(t: &Type) -> bool {
    is_set_elem(t)
}

/// Marker for the inner type of bare `none`. Implicitly converts to
/// any `Optional<T>` via `expect_type`. Users can't write this — `none`
pub fn is_none_inner(t: &Type) -> bool {
    matches!(t, Type::NoneInner)
}

pub fn is_empty_array_arg(t: &Type) -> bool {
    matches!(t, Type::EmptyArrayArg)
}

pub fn is_none_literal_arg(t: &Type) -> bool {
    matches!(t, Type::NoneLiteralArg)
}

// ---------- Builtin registry ----------

/// Builtins that only the AIPL-implemented builtins (`aipl-mono`'s
/// `builtin_*.aipl`) may import — declared and lowered like any other, but
/// not yet decided as part of the language a program can reach. `reserve` is
/// what `map_join` sizes its buffer with; whether a user should size their
/// own is an open question, and until it is answered the loader refuses the
/// import anywhere else ([`is_internal_builtin`]). `assume_nonempty` grants a
/// `T[] without []` with no test; `ensure_nonempty` is the one caller that
/// has made the test, and nothing else gets to skip it. `extend_optional` is one
/// *shape* of `extend` rather than an operation of its own — `extend` is the
/// spelling, and a public second name for the shape would be the hand-rolled
/// helper it replaced. `insert_key` is what `to_dict` and `map_to_dict` are
/// written with; `to_dict` is the way to build a dict from outside.
pub const INTERNAL_BUILTINS: &[Callee] = &[
    Callee::ShimCellGet,
    Callee::ShimCellSet,
    Callee::InsertKey,
    Callee::Reserve,
    Callee::AssumeNonempty,
    Callee::ExtendOptional,
    // The unchecked halves of `inline_str`: `pack_inline` has no answer for
    // content past the inline capacity, and the capacity is the bound its one
    // caller checks against. `inline_str` is the spelling.
    Callee::PackInline,
    Callee::InlineCapacity,
    // The counting window `measure` is built from. `measure` is the spelling.
    Callee::MeasureBegin,
    Callee::MeasureConsume,
    Callee::MeasureEnd,
    Callee::MeasureRead,
    // The unsigned half of `int_parse`, reached by the checker rewriting an
    // `int_parse` whose position wants an unsigned integer. `int_parse` is the
    // spelling.
    Callee::IntParseUnsigned,
];

/// Whether `callee` is one of [`INTERNAL_BUILTINS`].
pub fn is_internal_builtin(callee: &Callee) -> bool {
    INTERNAL_BUILTINS.contains(callee)
}

/// The *operations* of each shimmable effect: the builtins through which that
/// effect actually reaches its underlying resource. A `shim <effect> { .. }`
/// block must bind every one of them — a partial shim would let the body reach
/// the real resource while the enclosing function stops declaring the effect,
/// which would make that declaration a lie.
///
/// This is the one table that decides what can be shimmed. Every other part of
/// the feature (parser, checker, codegen, runtime slots) reads it rather than
/// naming a specific effect, so opening up another effect is an entry here, a
/// bump of [`SHIM_SLOT_COUNT`] and of both runtimes' slot arrays.
///
/// **Every effect is shimmable.** A new effect lists its operations here in the
/// same change that introduces it; `every_effect_is_shimmable` in
/// `tests/suites/shims.rs` fails otherwise. An operation is a builtin whose call
/// site codegen routes through its slot, whatever its signature.
///
/// New effects go at the end: the order assigns slot indices.
pub const SHIMMABLE_EFFECTS: &[(&str, &[&str])] = &[
    ("clock", &["now_nanos", "monotonic_now"]),
    ("prints", &["print", "put"]),
    ("read_files", &["read_file_to_string"]),
    ("read_stdin", &["read_line"]),
    ("write_files", &["write_string_to_file"]),
    ("list_files", &["list_files"]),
    ("execute_program", &["execute_program"]),
    ("random", &["random_u64"]),
    ("read_stdin_raw", &["read_byte"]),
    (
        "terminal",
        &[
            "clear_terminal",
            "move_to",
            "enter_alternate_screen",
            "leave_alternate_screen",
        ],
    ),
];

/// The effects that have no default implementation: the program must install one
/// with a `shim`. `main` cannot declare them — there is nothing for the real
/// operation to do — so they can only be discharged by a shim, and the checker
/// refuses a `main` that leaves one standing. Test bodies don't declare them
/// either; a test that needs one shims it.
pub const SHIM_ONLY_EFFECTS: &[&str] = &["random"];

/// Effects the language recognizes. `prints` = writes to stdout; `read_files` =
/// reads from the filesystem; `read_stdin` = reads standard input a line at a
/// time; `read_stdin_raw` = reads standard input a byte at a time, as it is typed;
/// `terminal` = controls the terminal itself — clears it, moves its cursor — which
/// is not the same as writing text to it (`prints`);
/// `write_files` = writes to the filesystem;
/// `list_files` = enumerates the filesystem (directory listing);
/// `execute_program` = spawns a child process; `clock` = reads the wall clock
/// (so a function's result depends on when it ran); `random` = draws random
/// numbers, which only a `shim` can supply ([`SHIM_ONLY_EFFECTS`]). `measure` is internal:
/// it marks `measure(..)` and the counting window it is built from, so only a
/// `.performance` block (which declares it, and nothing else) can reach them.
pub const KNOWN_EFFECTS: &[&str] = &[
    "prints",
    "read_files",
    "read_stdin",
    "read_stdin_raw",
    "terminal",
    "write_files",
    "list_files",
    "execute_program",
    "clock",
    "random",
    "measure",
];

/// The effects in [`KNOWN_EFFECTS`] that are compiler machinery rather than
/// something a program does to the world, and so have nothing to shim.
pub const INTERNAL_EFFECTS: &[&str] = &["measure"];

/// The operations of `effect`, or `None` if it isn't shimmable.
pub fn effect_operations(effect: &str) -> Option<&'static [&'static str]> {
    SHIMMABLE_EFFECTS
        .iter()
        .find(|(name, _)| *name == effect)
        .map(|(_, ops)| *ops)
}

/// Every shimmable operation across every effect, in declaration order — the
/// order that assigns each operation its runtime slot index (see
/// `shim_slot_index`). Stable because [`SHIMMABLE_EFFECTS`] is.
pub fn shim_operations() -> impl Iterator<Item = (&'static str, &'static str)> {
    SHIMMABLE_EFFECTS
        .iter()
        .flat_map(|(effect, ops)| ops.iter().map(move |op| (*effect, *op)))
}

/// The runtime slot index holding the installed shim for `op`, or `None` if `op`
/// isn't a shimmable operation. Both runtimes size their slot array by
/// [`SHIM_SLOT_COUNT`], so these indices are the shared contract.
pub fn shim_slot_index(op: &str) -> Option<usize> {
    shim_operations().position(|(_, name)| name == op)
}

/// How many shim slots the runtimes must reserve. Kept in sync by
/// `shim_slots_match_runtime` in `tests/shims.rs`.
pub const SHIM_SLOT_COUNT: usize = 15;

/// How many constants computed on first use ([`constant_initializer_name`]) a
/// program may have. Each owns a cache slot in the runtime, and both runtimes
/// size their slot array to this.
pub const CONST_SLOT_COUNT: usize = 256;

/// Built-in *type* names that must be brought into scope with
/// `import { .. } from builtins;` before use — the type-level analog of
/// [`IMPORTABLE_BUILTINS`]. Unlike the ambient builtin `Error` type (which
/// needs no import), these behave like any other importable builtin: gated,
/// and mapped to a reserved canonical name so a user's own type of the same
/// name can never silently collide.
pub const IMPORTABLE_BUILTIN_TYPES: &[&str] = &[
    "Span",
    "ExecResult",
    "Measurement",
    "Grid",
    "RandomProvider",
];

/// The canonical name of the builtin `Span` struct — what an imported `Span`
/// resolves to, and the name every pass that dispatches on "is this a range?"
/// compares against (iterating one, slicing by one, `map`ping one).
pub const SPAN_TYPE: &str = "__builtin_Span";

/// The name of the builtin generic struct behind `T[,]`.
pub const GRID_TYPE: &str = "__builtin_Grid";

/// Whether `name` is an instance of the builtin grid struct — `__builtin_Grid$i64`
/// for `i64[,]`.
pub fn is_grid_instance(name: &str) -> bool {
    name.strip_prefix(GRID_TYPE)
        .is_some_and(|rest| rest.starts_with('$'))
}

/// Canonical internal name for an importable builtin type, or `None` if
/// `name` isn't one. Mirrors [`builtin_canonical`] for types: the loader
/// rewrites an imported `Span` to `__builtin_Span`, the name
/// [`BUILTIN_SIGNATURES`] actually declares the struct under.
pub fn builtin_type_canonical(name: &str) -> Option<String> {
    if IMPORTABLE_BUILTIN_TYPES.contains(&name) {
        Some(format!("__builtin_{name}"))
    } else {
        None
    }
}

/// Visit every expression in `program` — function bodies, `.test` blocks, and
/// keyword-parameter / struct-field / variant-payload default expressions —
/// pre-order.
pub fn each_expr(program: &ast::Program, f: &mut impl FnMut(&ast::Expr)) {
    each_item_expr(&program.items, f);
}

/// [`each_expr`] over `items`, which need not make up a program.
pub fn each_item_expr(items: &[ast::Item], f: &mut impl FnMut(&ast::Expr)) {
    for item in items {
        match item {
            ast::Item::Fn(func) => {
                for p in &func.sig.params {
                    if let Some(d) = &p.default {
                        each_subexpr(d, f);
                    }
                }
                each_subexpr(&func.body, f);
                if let Some(t) = &func.test_body {
                    each_subexpr(t, f);
                }
            }
            ast::Item::Struct(s) => {
                for field in &s.fields {
                    if let Some(d) = &field.default {
                        each_subexpr(d, f);
                    }
                }
            }
            ast::Item::Variant(v) => {
                for case in &v.cases {
                    for slot in &case.payload {
                        if let Some(d) = &slot.default {
                            each_subexpr(d, f);
                        }
                    }
                }
            }
            ast::Item::Const(c) => each_subexpr(&c.value, f),
            ast::Item::Refinement(_) | ast::Item::Import(_) => {}
        }
    }
}

/// The type under any `without` refinement — `T[]` for `T[] without []`, and
/// `t` itself when it carries none. What a refined value *is* at runtime, and
/// so the type to ask structural questions of: whether it can be indexed,
/// walked, sliced, matched against `[..]` patterns.
/// Whether a `for` loop's binders include `name`: the element binder, or the
/// index binder of the two-binder form. Every pass that asks whether a name is
/// shadowed inside a loop body asks it here, so neither binder can be forgotten
/// at one site and remembered at another.
pub fn for_binds(var: &str, index: &Option<String>, name: &str) -> bool {
    var == name || index.as_deref() == Some(name)
}

/// A `for` loop's binders as a list — the element one, plus the index one when
/// the two-binder form was written. For the passes that take a set of names
/// rather than ask about one.
pub fn loop_binders(var: &str, index: &Option<String>) -> Vec<String> {
    let mut v = vec![var.to_string()];
    if let Some(i) = index {
        v.push(i.clone());
    }
    v
}

pub fn unrefined(t: &Type) -> &Type {
    match t {
        Type::Without(base, _) => unrefined(base),
        _ => t,
    }
}

/// `t` with every refinement, at any depth, replaced by its base:
/// `(T[] without [])?` becomes `T[]?`. See [`erase_refinements`].
pub fn erase_refinement_ty(t: &Type) -> Type {
    use ast::Type as T;
    match t {
        T::Without(base, _) => erase_refinement_ty(base),
        T::Unit
        | T::Primitive(_)
        | T::Named(_)
        | T::TypeVar(_)
        | T::Any
        | T::NoneInner
        | T::EmptyArrayArg
        | T::NoneLiteralArg
        | T::Unknown
        | T::ConcatStr => t.clone(),
        T::Case(v) => T::Case(Box::new(erase_refinement_ty(v))),
        T::Optional(i) => T::Optional(Box::new(erase_refinement_ty(i))),
        T::Array(i) => T::Array(Box::new(erase_refinement_ty(i))),
        T::Set(i, o) => T::Set(Box::new(erase_refinement_ty(i)), *o),
        T::Dict(k, v) => T::Dict(
            Box::new(erase_refinement_ty(k)),
            Box::new(erase_refinement_ty(v)),
        ),
        T::Result(a, b) => T::Result(
            Box::new(erase_refinement_ty(a)),
            Box::new(erase_refinement_ty(b)),
        ),
        T::Fn(ps, r) => T::Fn(
            ps.iter().map(erase_refinement_ty).collect(),
            Box::new(erase_refinement_ty(r)),
        ),
        T::Tuple(es) => T::Tuple(es.iter().map(erase_refinement_ty).collect()),
        T::Generic(n, args) => {
            T::Generic(n.clone(), args.iter().map(erase_refinement_ty).collect())
        }
    }
}

/// Every `without` refinement in `program` replaced by its base type — in
/// signatures, field and payload declarations, `let`/`mut` annotations, lambda
/// parameters, and the types the checker stamped on expressions — and every
/// `assume_nonempty(x)` call, which only introduced one, replaced by `x`.
///
/// A refinement is a promise the checker has finished verifying by the time it
/// hands the program on, and it is not a representation: `T[] without []` is
/// laid out, retained and dropped exactly as `T[]` is. Erasing it here is what
/// lets monomorphization and codegen match on the base types alone, without an
/// arm apiece for a distinction that makes no difference to them.
pub fn erase_refinements(program: &mut ast::Program) {
    each_type_mut(program, &mut |t| *t = erase_refinement_ty(t));
    each_body_mut(program, &mut erase_assumptions);
}

/// [`erase_refinements`] for the `assume_nonempty` calls in one expression
/// tree: each becomes its argument.
fn erase_assumptions(e: &mut ast::Expr) {
    for child in each_subexpr_mut(e) {
        erase_assumptions(child);
    }
    if let ast::ExprKind::Call(Callee::AssumeNonempty, args, _) = &mut e.kind {
        if args.len() == 1 {
            *e = args.pop().expect("one argument");
        }
    }
}

/// Substitute template type variables in `t` with concrete types from `map`,
/// recursing through every compound type — including nested generic
/// applications, so a generic field like `inner: Box<T>` becomes `Box<i64>`
/// once `T` is bound. A `Named` not in `map` (a concrete type or an unrelated
/// name) is left unchanged.
pub fn subst_type_params(t: &Type, map: &HashMap<String, Type>) -> Type {
    match t {
        Type::Case(v) => Type::Case(Box::new(subst_type_params(v, map))),
        // Only a *variable* substitutes. Keying this on `Named` meant a struct
        // that happened to share a template parameter's name was rewritten too.
        Type::TypeVar(n) => map.get(n).cloned().unwrap_or_else(|| t.clone()),
        Type::Named(_) | Type::Unknown => t.clone(),
        Type::Optional(i) => Type::Optional(Box::new(subst_type_params(i, map))),
        Type::Array(i) => Type::Array(Box::new(subst_type_params(i, map))),
        Type::Without(b, x) => Type::Without(Box::new(subst_type_params(b, map)), x.clone()),
        Type::Set(i, o) => Type::Set(Box::new(subst_type_params(i, map)), *o),
        Type::Dict(k, v) => Type::Dict(
            Box::new(subst_type_params(k, map)),
            Box::new(subst_type_params(v, map)),
        ),
        Type::Result(ok, err) => Type::Result(
            Box::new(subst_type_params(ok, map)),
            Box::new(subst_type_params(err, map)),
        ),
        Type::Fn(ps, r) => Type::Fn(
            ps.iter().map(|p| subst_type_params(p, map)).collect(),
            Box::new(subst_type_params(r, map)),
        ),
        Type::Tuple(es) => Type::Tuple(es.iter().map(|e| subst_type_params(e, map)).collect()),
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter().map(|a| subst_type_params(a, map)).collect(),
        ),
        Type::Unit
        | Type::Primitive(_)
        | Type::Any
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => t.clone(),
    }
}

/// Every mention of a named refinement (`variant W = V without C`) replaced by
/// the type it stands for, and the declarations themselves dropped — so what
/// leaves the loader holds only the [`ast::Type::Without`] they spell, and
/// nothing past it knows a refinement can be named.
///
/// A refinement may refine another (`variant X = W without D`), so each target
/// is expanded against the rest, and one that reaches itself is refused. On an
/// error the name of the declaration is returned beside it, so the caller can
/// attribute the diagnostic to the file that declared it.
///
/// The arm types of a variant are expanded the same way, from declarations the
/// program never wrote: see [`arm_type_decls`]. They are not among the
/// declarations returned, which are the ones to hold to the rules for a
/// `variant W = V without C` — an arm type is `V` less every case but one
/// by construction.
///
/// Each declaration's own target comes back fully expanded, used or not: a
/// declaration nothing mentions leaves no type behind for the checker to
/// validate, so the caller holds it to the same rules from this.
pub fn expand_refinements(
    program: &mut ast::Program,
) -> Result<Vec<ExpandedRefinement>, (String, Error)> {
    let mut decls: HashMap<String, ast::RefinementDecl> = HashMap::new();
    program.items.retain(|item| match item {
        ast::Item::Refinement(r) => {
            decls.insert(r.name.text.clone(), r.clone());
            false
        }
        _ => true,
    });
    // In source order, so which of several faults is reported first does not
    // depend on the map.
    let mut written: Vec<&ast::RefinementDecl> = decls.values().collect();
    written.sort_by_key(|d| d.name.span.start);
    let written: Vec<String> = written.iter().map(|d| d.name.text.clone()).collect();
    for arm in arm_type_decls(program) {
        decls.insert(arm.name.text.clone(), arm);
    }
    if decls.is_empty() {
        return Ok(Vec::new());
    }
    let mut declared = Vec::new();
    for decl in written.iter().map(|name| &decls[name]) {
        // Seeded with the declaration itself, so one that reaches itself
        // through its own target is found from here as well as from a use.
        let mut stack = vec![decl.name.text.clone()];
        let target = expand_refinement_ty(&decl.target, &decls, &mut stack)?;
        declared.push(ExpandedRefinement {
            name: decl.name.clone(),
            target,
        });
    }
    let mut first_error = None;
    each_type_mut(program, &mut |t| {
        if first_error.is_none() {
            match expand_refinement_ty(t, &decls, &mut Vec::new()) {
                Ok(expanded) => *t = expanded,
                Err(e) => first_error = Some(e),
            }
        }
    });
    first_error.map_or(Ok(declared), Err)
}

/// The declarations of the arm types `program` writes: a case's name where a
/// type goes, which the loader resolves to the case-qualified `Case@Variant` it
/// resolves a constructor to. An arm type is its variant less every other case —
/// the type `xs.collect_arms(Case)` has for its elements — so a `match` on a
/// value of it needs that arm and no other, and the value goes wherever its
/// variant does.
///
/// Only the arm types the program mentions are declared, found by reading every
/// type it holds. A generic variant's arm type takes the variant's own type
/// arguments (`Some<i64>`), which is what declaring it with the variant's type
/// parameters makes it do.
fn arm_type_decls(program: &mut ast::Program) -> Vec<ast::RefinementDecl> {
    fn qualified_names(t: &Type, out: &mut std::collections::HashSet<String>) {
        match t {
            Type::Named(n) if n.contains('@') => {
                out.insert(n.clone());
            }
            Type::Generic(n, args) => {
                if n.contains('@') {
                    out.insert(n.clone());
                }
                args.iter().for_each(|a| qualified_names(a, out));
            }
            Type::Case(i)
            | Type::Optional(i)
            | Type::Array(i)
            | Type::Without(i, _)
            | Type::Set(i, _) => qualified_names(i, out),
            Type::Dict(a, b) | Type::Result(a, b) => {
                qualified_names(a, out);
                qualified_names(b, out);
            }
            Type::Fn(ps, r) => {
                ps.iter().for_each(|p| qualified_names(p, out));
                qualified_names(r, out);
            }
            Type::Tuple(es) => es.iter().for_each(|e| qualified_names(e, out)),
            Type::Unit
            | Type::Primitive(_)
            | Type::Named(_)
            | Type::TypeVar(_)
            | Type::Any
            | Type::NoneInner
            | Type::EmptyArrayArg
            | Type::NoneLiteralArg
            | Type::Unknown
            | Type::ConcatStr => {}
        }
    }
    let mut written = std::collections::HashSet::new();
    each_type_mut(program, &mut |t| qualified_names(t, &mut written));
    if written.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for item in &program.items {
        let ast::Item::Variant(v) = item else {
            continue;
        };
        for case in &v.cases {
            let key = format!("{}@{}", case.name.text, v.name.text);
            if !written.contains(&key) {
                continue;
            }
            let base = if v.is_generic() {
                Type::Generic(
                    v.name.text.clone(),
                    v.type_vars
                        .iter()
                        .map(|p| Type::TypeVar(p.name.clone()))
                        .collect(),
                )
            } else {
                Type::Named(v.name.text.clone())
            };
            // Folded in declaration order, as `collect_arms` types its result,
            // so the two spell one type.
            let target = v
                .cases
                .iter()
                .filter(|c| c.name.text != case.name.text)
                .fold(base, |acc, c| {
                    Type::Without(
                        Box::new(acc),
                        ast::Exclusion::NamedCase(c.name.text.clone()),
                    )
                });
            out.push(ast::RefinementDecl {
                name: ast::SpanStr {
                    text: key,
                    span: case.name.span.clone(),
                },
                doc: None,
                type_vars: v.type_vars.clone(),
                target,
            });
        }
    }
    out
}

/// A refinement declaration with its target expanded — see
/// [`expand_refinements`].
#[derive(Debug, Clone)]
pub struct ExpandedRefinement {
    pub name: ast::SpanStr,
    pub target: Type,
}

/// [`expand_refinements`] for one type: `t` with every named refinement in it
/// replaced, recursively. `stack` holds the refinements being expanded, which is
/// how a cycle is told from a chain.
fn expand_refinement_ty(
    t: &Type,
    decls: &HashMap<String, ast::RefinementDecl>,
    stack: &mut Vec<String>,
) -> Result<Type, (String, Error)> {
    use ast::Type as T;
    let mut sub = |t: &Type| expand_refinement_ty(t, decls, stack);
    Ok(match t {
        T::Named(n) | T::Generic(n, _) if decls.contains_key(n) => {
            let decl = &decls[n];
            let args: Vec<Type> = match t {
                T::Generic(_, args) => args.iter().map(&mut sub).collect::<Result<_, _>>()?,
                _ => Vec::new(),
            };
            let fail = |message: String| (n.clone(), Error::at(message, decl.name.span.clone()));
            if args.len() != decl.type_vars.len() {
                return Err(fail(format!(
                    "{:?} takes {} type argument(s), but {} given",
                    decl.name.text,
                    decl.type_vars.len(),
                    args.len()
                )));
            }
            if stack.contains(n) {
                return Err(fail(format!(
                    "{:?} refines itself: {} -> {n}",
                    decl.name.text,
                    stack.join(" -> ")
                )));
            }
            let map: HashMap<String, Type> = decl
                .type_vars
                .iter()
                .map(|v| v.name.clone())
                .zip(args)
                .collect();
            stack.push(n.clone());
            let target = expand_refinement_ty(&subst_type_params(&decl.target, &map), decls, stack);
            stack.pop();
            target?
        }
        T::Case(v) => T::Case(Box::new(sub(v)?)),
        T::Optional(i) => T::Optional(Box::new(sub(i)?)),
        T::Array(i) => T::Array(Box::new(sub(i)?)),
        T::Without(b, x) => T::Without(Box::new(sub(b)?), x.clone()),
        T::Set(i, o) => T::Set(Box::new(sub(i)?), *o),
        T::Dict(k, v) => T::Dict(Box::new(sub(k)?), Box::new(sub(v)?)),
        T::Result(a, b) => T::Result(Box::new(sub(a)?), Box::new(sub(b)?)),
        T::Fn(ps, r) => T::Fn(
            ps.iter().map(&mut sub).collect::<Result<_, _>>()?,
            Box::new(sub(r)?),
        ),
        T::Tuple(es) => T::Tuple(es.iter().map(&mut sub).collect::<Result<_, _>>()?),
        T::Generic(n, args) => T::Generic(
            n.clone(),
            args.iter().map(&mut sub).collect::<Result<_, _>>()?,
        ),
        T::Unit
        | T::Primitive(_)
        | T::Named(_)
        | T::TypeVar(_)
        | T::Any
        | T::NoneInner
        | T::EmptyArrayArg
        | T::NoneLiteralArg
        | T::Unknown
        | T::ConcatStr => t.clone(),
    })
}

/// Every type written or recorded in `program`, mutably: signatures, field and
/// payload declarations, and the annotations inside bodies, plus each
/// expression's stamped [`Expr::ty`].
fn each_type_mut(program: &mut ast::Program, f: &mut impl FnMut(&mut Type)) {
    for item in &mut program.items {
        match item {
            ast::Item::Fn(func) => {
                for p in &mut func.sig.params {
                    f(&mut p.ty);
                }
                if let Some(r) = &mut func.sig.return_ty {
                    f(r);
                }
            }
            ast::Item::Struct(s) => {
                for fd in &mut s.fields {
                    f(&mut fd.ty);
                }
            }
            ast::Item::Variant(v) => {
                for case in &mut v.cases {
                    for slot in &mut case.payload {
                        f(&mut slot.ty);
                    }
                }
            }
            ast::Item::Const(c) => {
                if let Some(t) = &mut c.ty {
                    f(t);
                }
            }
            ast::Item::Refinement(r) => f(&mut r.target),
            ast::Item::Import(_) => {}
        }
    }
    each_body_mut(program, &mut |e| each_type_in_expr_mut(e, f));
}

/// Every expression tree in `program`, mutably: function bodies and test
/// bodies, and the default expressions of struct fields and variant payloads.
fn each_body_mut(program: &mut ast::Program, f: &mut impl FnMut(&mut ast::Expr)) {
    for item in &mut program.items {
        match item {
            ast::Item::Fn(func) => {
                f(&mut func.body);
                if let Some(t) = &mut func.test_body {
                    f(t);
                }
            }
            ast::Item::Struct(s) => {
                for fd in &mut s.fields {
                    if let Some(d) = &mut fd.default {
                        f(d);
                    }
                }
            }
            ast::Item::Variant(v) => {
                for case in &mut v.cases {
                    for slot in &mut case.payload {
                        if let Some(d) = &mut slot.default {
                            f(d);
                        }
                    }
                }
            }
            ast::Item::Const(c) => f(&mut c.value),
            ast::Item::Refinement(_) | ast::Item::Import(_) => {}
        }
    }
}

/// [`each_type_mut`] within one expression tree.
fn each_type_in_expr_mut(e: &mut ast::Expr, f: &mut impl FnMut(&mut Type)) {
    use ast::ExprKind as K;
    if let Some(t) = &mut e.ty {
        f(t);
    }
    match &mut e.kind {
        K::Let(_, ty, _, _) | K::LetMut(_, ty, _, _) => {
            if let Some(t) = ty {
                f(t);
            }
        }
        K::Lambda(params, _) => {
            for p in params {
                if let Some(t) = &mut p.ty {
                    f(t);
                }
            }
        }
        K::TypeArgs(_, tys) => {
            for t in tys {
                f(t);
            }
        }
        _ => {}
    }
    for child in each_subexpr_mut(e) {
        each_type_in_expr_mut(child, f);
    }
}

/// The direct sub-expressions of `e`, mutably — the shape [`each_subexpr`]
/// walks, for passes that rewrite rather than inspect.
pub fn each_subexpr_mut(e: &mut ast::Expr) -> Vec<&mut ast::Expr> {
    use ast::ExprKind as K;
    match &mut e.kind {
        K::Num(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::Ident(_) | K::None | K::Unit => {
            vec![]
        }
        K::Shim(_, _, body) => vec![body.as_mut()],
        K::Call(_, args, _) | K::ArrayLit(args) | K::SetLit(args, _) | K::TupleLit(args) => {
            args.iter_mut().collect()
        }
        K::Construct(_, inits) => inits.iter_mut().map(|i| &mut i.value).collect(),
        K::DictLit(pairs) => pairs.iter_mut().flat_map(|(k, v)| [k, v]).collect(),
        K::Seq(a, b)
        | K::Index(a, b)
        | K::Let(_, _, a, b)
        | K::LetMut(_, _, a, b)
        | K::For(_, _, a, b)
        | K::While(a, b) => vec![a.as_mut(), b.as_mut()],
        K::Assign(a, b, c) | K::If(a, b, c) => vec![a.as_mut(), b.as_mut(), c.as_mut()],
        K::Neg(x)
        | K::Field(x, _)
        | K::Try(x)
        | K::Return(x)
        | K::KwArg(_, x, _)
        | K::TypeArgs(x, _)
        | K::Spread(x)
        | K::Lambda(_, x) => vec![x.as_mut()],
        K::Match(scrutinee, arms) => {
            let mut v = vec![scrutinee.as_mut()];
            v.extend(arms.iter_mut().map(|a| &mut a.body));
            v
        }
        K::IfLet(arm, scrutinee, else_b) => {
            vec![scrutinee.as_mut(), &mut arm.body, else_b.as_mut()]
        }
        K::Slice(a, b, c) => {
            let mut v = vec![a.as_mut(), b.as_mut()];
            if let Some(c) = c {
                v.push(c.as_mut());
            }
            v
        }
    }
}

/// What the parser names the function a shim binds a closure to: the closure is
/// the value of a `let` of this name around the shim, and the shim binds the
/// operation to the name. Everything between the parser and `aipl-mono`'s lowering
/// of closure shims (`shim_closures`) sees a closure as that `let`.
pub const SHIM_CLOSURE_PREFIX: &str = "$shimfn$";

/// Whether `name` is the name the parser gave a shim's closure.
pub fn is_shim_closure_name(name: &str) -> bool {
    name.starts_with(SHIM_CLOSURE_PREFIX)
}

/// The closures a shim binds and the shim itself: `e` is a chain of `let`s of
/// shim-closure names (one per closure the shim binds) around a [`ShimExpr`].
/// `None` for any other expression.
///
/// [`ShimExpr`]: ast::ExprKind::Shim
pub fn shim_closure_chain(e: &ast::Expr) -> Option<(Vec<(&str, &ast::Expr)>, &ast::Expr)> {
    let mut closures = Vec::new();
    let mut cur = e;
    while let ast::ExprKind::Let(name, _, closure, rest) = &cur.kind {
        if !is_shim_closure_name(name) {
            return None;
        }
        closures.push((name.as_str(), closure.as_ref()));
        cur = rest;
    }
    match cur.kind {
        ast::ExprKind::Shim(..) if !closures.is_empty() => Some((closures, cur)),
        _ => None,
    }
}

/// The variables `set` anywhere in `e`: the root name of every assignment target
/// (`set a.b.c = ..` and `set a.push(..)` both name `a`), in order of first
/// assignment.
pub fn assigned_names(e: &ast::Expr) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    each_subexpr(e, &mut |x| {
        if let ast::ExprKind::Assign(lhs, _, _) = &x.kind {
            if let Some((name, _)) = ast::assign_target(lhs) {
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
        }
    });
    names
}

/// Whether evaluating `e` always leaves the function: it is a `return`, or a
/// sequence one of whose steps is, or a branching form every branch of which is.
/// Decided from the shape alone — `?` can leave but may not, so it does not count.
///
/// A branch that diverges has no value to give the form around it, which is why
/// an `if` or `if let` is typed by its other branch alone.
pub fn diverges(e: &ast::Expr) -> bool {
    use ast::ExprKind as K;
    match &e.kind {
        K::Return(_) => true,
        K::Seq(first, rest) => diverges(first) || diverges(rest),
        K::Let(_, _, _, rest) | K::LetMut(_, _, _, rest) | K::Assign(_, _, rest) => diverges(rest),
        K::If(_, then, otherwise) => diverges(then) && diverges(otherwise),
        K::IfLet(arm, _, otherwise) => diverges(&arm.body) && diverges(otherwise),
        K::Match(_, arms) => !arms.is_empty() && arms.iter().all(|a| diverges(&a.body)),
        _ => false,
    }
}

pub fn each_subexpr(e: &ast::Expr, f: &mut impl FnMut(&ast::Expr)) {
    use ast::ExprKind as K;
    f(e);
    match &e.kind {
        K::Num(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::Ident(_) | K::None | K::Unit => {}
        // A shim's bindings name functions; only its body nests expressions.
        K::Shim(_, _, body) => each_subexpr(body, f),
        K::Call(_, args, _) | K::ArrayLit(args) | K::SetLit(args, _) | K::TupleLit(args) => {
            for a in args {
                each_subexpr(a, f);
            }
        }
        K::Construct(_, inits) => {
            for init in inits {
                each_subexpr(&init.value, f);
            }
        }
        K::DictLit(pairs) => {
            for (k, v) in pairs {
                each_subexpr(k, f);
                each_subexpr(v, f);
            }
        }
        K::Seq(a, b)
        | K::Index(a, b)
        | K::Let(_, _, a, b)
        | K::LetMut(_, _, a, b)
        | K::For(_, _, a, b)
        | K::While(a, b) => {
            each_subexpr(a, f);
            each_subexpr(b, f);
        }
        K::Assign(a, b, c) | K::If(a, b, c) => {
            each_subexpr(a, f);
            each_subexpr(b, f);
            each_subexpr(c, f);
        }
        K::Neg(x)
        | K::Field(x, _)
        | K::Try(x)
        | K::Return(x)
        | K::KwArg(_, x, _)
        | K::TypeArgs(x, _)
        | K::Spread(x)
        | K::Lambda(_, x) => each_subexpr(x, f),
        K::Match(scrutinee, arms) => {
            each_subexpr(scrutinee, f);
            for arm in arms {
                each_subexpr(&arm.body, f);
            }
        }
        K::IfLet(arm, scrutinee, else_b) => {
            each_subexpr(scrutinee, f);
            each_subexpr(&arm.body, f);
            each_subexpr(else_b, f);
        }
        K::Slice(a, b, c) => {
            each_subexpr(a, f);
            each_subexpr(b, f);
            if let Some(c) = c {
                each_subexpr(c, f);
            }
        }
    }
}

/// The names `e` reads from the scope *around* it — every identifier and
/// called-function name that isn't bound by a `let`/`mut`/`for`/lambda/match
/// arm inside `e` itself.
///
/// Scope-aware, which is what separates it from a bare [`each_subexpr`] sweep:
/// `{ let a = 1; a }` reads nothing, so a caller asking "does this expression
/// mention the parameter `a`?" gets `false` rather than a false positive. The
/// loader asks exactly that of a keyword parameter's default, where the answer
/// decides whether a call-site argument has to be bound to a temporary.
///
/// A `Call`'s callee counts as a read: it may name a function-typed local
/// rather than a global. A `Construct`'s type name does not — it names a
/// struct, not a value.
pub fn free_idents(e: &ast::Expr) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    collect_free(e, &mut Vec::new(), &mut out);
    out
}

/// [`free_idents`]'s recursion. `bound` is the stack of names in scope at `e`,
/// innermost last; a shadowing binder pushes and pops around its body.
fn collect_free(
    e: &ast::Expr,
    bound: &mut Vec<String>,
    out: &mut std::collections::HashSet<String>,
) {
    use ast::ExprKind as K;
    // A name not shadowed by an enclosing binder is read from outside `e`.
    fn read(n: &str, bound: &[String], out: &mut std::collections::HashSet<String>) {
        if !bound.iter().any(|b| b == n) {
            out.insert(n.to_string());
        }
    }
    match &e.kind {
        K::Ident(n) => read(n, bound, out),
        K::Call(n, args, _) => {
            read(n.name(), bound, out);
            for a in args {
                collect_free(a, bound, out);
            }
        }
        // `name` is in scope for the body only, never for the value.
        K::Let(name, _, value, body) | K::LetMut(name, _, value, body) => {
            collect_free(value, bound, out);
            bound.push(name.clone());
            collect_free(body, bound, out);
            bound.pop();
        }
        K::For(var, index, iterable, body) => {
            collect_free(iterable, bound, out);
            bound.push(var.clone());
            // The index binder scopes over the body exactly as the element one
            // does, so a body mentioning it is not mentioning a free variable.
            if let Some(i) = index {
                bound.push(i.clone());
            }
            collect_free(body, bound, out);
            bound.pop();
        }
        K::Lambda(params, body) => {
            let depth = bound.len();
            bound.extend(params.iter().map(|p| p.name.clone()));
            collect_free(body, bound, out);
            bound.truncate(depth);
        }
        K::Match(scrutinee, arms) => {
            collect_free(scrutinee, bound, out);
            for arm in arms {
                let depth = bound.len();
                bound.extend(arm.pattern.bindings());
                collect_free(&arm.body, bound, out);
                bound.truncate(depth);
            }
        }
        // The pattern's bindings are in scope for `arm.body` (the `then`
        // branch) only — not the scrutinee it's read from, and not `else_b`.
        K::IfLet(arm, scrutinee, else_b) => {
            collect_free(scrutinee, bound, out);
            let depth = bound.len();
            bound.extend(arm.pattern.bindings());
            collect_free(&arm.body, bound, out);
            bound.truncate(depth);
            collect_free(else_b, bound, out);
        }
        _ => {
            for child in children(e) {
                collect_free(child, bound, out);
            }
        }
    }
}

/// `e`'s immediate sub-expressions — the shared-shape half of
/// [`collect_free`], whose own arms cover every node that *binds* a name.
/// Mirrors [`each_subexpr_mut`].
fn children(e: &ast::Expr) -> Vec<&ast::Expr> {
    use ast::ExprKind as K;
    match &e.kind {
        K::Num(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::Ident(_) | K::None | K::Unit => {
            vec![]
        }
        K::Shim(_, _, body) => vec![body],
        K::Call(_, args, _) | K::ArrayLit(args) | K::SetLit(args, _) | K::TupleLit(args) => {
            args.iter().collect()
        }
        K::Construct(_, inits) => inits.iter().map(|i| &i.value).collect(),
        K::DictLit(pairs) => pairs.iter().flat_map(|(k, v)| [k, v]).collect(),
        K::Seq(a, b)
        | K::Index(a, b)
        | K::Let(_, _, a, b)
        | K::LetMut(_, _, a, b)
        | K::For(_, _, a, b)
        | K::While(a, b) => vec![a, b],
        K::Assign(a, b, c) | K::If(a, b, c) => vec![a, b, c],
        K::Neg(x)
        | K::Field(x, _)
        | K::Try(x)
        | K::Return(x)
        | K::KwArg(_, x, _)
        | K::TypeArgs(x, _)
        | K::Spread(x)
        | K::Lambda(_, x) => vec![x],
        K::Match(scrutinee, arms) => {
            let mut v = vec![&**scrutinee];
            v.extend(arms.iter().map(|a| &a.body));
            v
        }
        K::IfLet(arm, scrutinee, else_b) => vec![scrutinee, &arm.body, else_b],
        K::Slice(a, b, c) => {
            let mut v = vec![&**a, &**b];
            if let Some(c) = c {
                v.push(c);
            }
            v
        }
    }
}

/// Whether `t` mentions a generic type parameter — a declared `<T>` or the
/// anonymous `any` — and so isn't a runtime type until monomorphization has
/// substituted it. A binding cannot be annotated with one.
pub fn mentions_typevar(t: &ast::Type) -> bool {
    use ast::Type as T;
    match t {
        T::TypeVar(_) | T::Any => true,
        T::Case(v) | T::Optional(v) | T::Array(v) | T::Set(v, _) | T::Without(v, _) => {
            mentions_typevar(v)
        }
        T::Dict(k, v) | T::Result(k, v) => mentions_typevar(k) || mentions_typevar(v),
        T::Fn(ps, r) => ps.iter().any(mentions_typevar) || mentions_typevar(r),
        T::Tuple(es) | T::Generic(_, es) => es.iter().any(mentions_typevar),
        T::Unit
        | T::Primitive(_)
        | T::Named(_)
        | T::NoneInner
        | T::EmptyArrayArg
        // Not a type parameter: a wildcard is what the checker reaches for when
        // it *cannot* pin a type down, not a parameter waiting to be substituted.
        | T::NoneLiteralArg
        | T::Unknown
        | T::ConcatStr => false,
    }
}

/// Prefix of the binding the loader synthesizes for a `..base` struct spread
/// (`aipl-loader`'s `desugar_struct_spread`). It is the one thing that ties the
/// two ends of that desugaring together: the loader emits the binding carrying
/// the target struct's type, and the checker recognizes it so a failed
/// annotation is reported as the spread the user actually wrote rather than as
/// an internal `let`. Shared rather than spelled twice, because a silent
/// mismatch would turn a good diagnostic back into a confusing one.
pub const SPREAD_BASE_PREFIX: &str = "__spread_base$";

/// Prefix of the binding the loader hoists a call-site argument into when a
/// keyword parameter's default reads it (`aipl-loader`'s `expand_call_args`).
/// The default is spliced in at the call site and mentions the argument once
/// per read, so anything but a name or a literal is evaluated into one of these
/// first — `f(g())` with `fn f(a: T, b: T = a)` must not call `g` twice. Same
/// job as [`SPREAD_BASE_PREFIX`], for the same reason.
pub const KWARG_ARG_PREFIX: &str = "__kwarg$";

/// Name prefix the loader gives an *unnamed* variant payload slot
/// (`Circle(i64)`) so a case's payload can be handled as a parameter list. The
/// `$` keeps it unwritable in source, which is the point: an unnamed slot is
/// positional-only and nothing may read it by name.
pub const CASE_SLOT_PREFIX: &str = "__slot$";

/// Prefix of the temporary a `let T { a, b } = value;` destructuring binds its
/// scrutinee to (the parser's `StmtSpec::LetStruct` lowering). It plays the same
/// two roles as [`SPREAD_BASE_PREFIX`]: the binding carries `T` as its type
/// annotation, so the checker rejects a pattern naming a *different* struct than
/// the value is — two structs that merely share field names are not
/// interchangeable — and the prefix is what lets the checker report that as the
/// destructuring the user wrote rather than as an internal `let`.
///
/// The loader drops the annotation when `T` names a *generic* struct, for the
/// reason the spread does: `Box` names a template, not a type, so there is
/// nothing to pin it to until monomorphization. The field reads are still
/// checked, so a wrong field name is caught either way; what goes unchecked for
/// a generic is a different template that happens to share field names.
pub const DESTRUCTURE_BASE_PREFIX: &str = "__spat$";

/// Name prefix of the temporary a tuple pattern in `let (a, b) = v;` reads its
/// slots from (`__tpat$<offset>._0`, `._1`, ..). The checker uses it to report a
/// value that is no tuple as the pattern the user wrote, not as a field access
/// on a binding they never named. The grammar spells it too
/// (`grammar_aipl.aipl`, `lower_let_tuple`).
pub const LET_TUPLE_BASE_PREFIX: &str = "__tpat$";

/// [`LET_TUPLE_BASE_PREFIX`] for the loop form, `for (let (a, b) : xs)`, where
/// the temporary is the loop variable and the value in question is each element.
pub const FOR_TUPLE_BASE_PREFIX: &str = "__fpat$";

/// Parameter-name prefix of the lambda `aipl_mono::lower_ctor_refs` wraps a
/// payload-carrying constructor reference in (`Circle` → `|__ctor0| Circle(__ctor0)`).
/// Shared so [`ctor_ref_case`] can recognize that lambda and read the case back
/// out of it — the two ends of one desugaring, like [`SPREAD_BASE_PREFIX`].
pub const CTOR_PARAM_PREFIX: &str = "__ctor";

/// Name prefix of the shared function `aipl_mono::lower_ctor_refs` synthesizes
/// for a payload-carrying constructor of a *non-generic* variant, so that every
/// reference to it becomes one address rather than a lambda apiece. The other
/// end of that desugaring is [`ctor_wrapper_name`] and [`ctor_ref_case`].
pub const CTOR_WRAPPER_PREFIX: &str = "__ctor$";

/// The shared wrapper function's name for case `case` of variant `variant`.
/// Neither name can contain a `$` at this stage — a variant is named by its
/// template, and only monomorphization introduces `Instance$Arg` — so the two
/// halves split back apart unambiguously.
pub fn ctor_wrapper_name(variant: &str, case: &str) -> String {
    format!("{CTOR_WRAPPER_PREFIX}{variant}${case}")
}

/// The case a construction expression builds, and the variant it belongs to —
/// `("A", "V")` for `A@V` written bare or applied (`A@V(1)`). `None` for
/// anything that is not a construction, which includes a constructor used as a
/// *value* (`xs.map(Circle)`): that is a function, not a case of anything.
pub fn constructed_case(e: &ast::Expr) -> Option<(&str, &str)> {
    let name = match &e.kind {
        ast::ExprKind::Ident(n) => n.as_str(),
        ast::ExprKind::Call(callee, _, _) => callee.name(),
        _ => return None,
    };
    // A constructor *reference* used as a value (`xs.map(Circle)`) is not a
    // construction, and neither is a function whose name happens to be bare.
    if name.starts_with(CTOR_WRAPPER_PREFIX) {
        return None;
    }
    name.split_once('@')
}

/// The `(case, variant)` a constructor *reference* names, if `e` is one.
///
/// Two shapes reach here, because a reference is lowered differently by arity: a
/// nullary constructor stays a bare `Ident`, while a payload-carrying one is
/// already the lambda `lower_ctor_refs` made of it. Both still name the case, and
/// the loader's variant-qualified `Case@Variant` form carries the variant along
/// with it — so this needs no symbol table, which is what lets it live here and
/// be used from [`flex_fit`].
///
/// This is how `Case<V>` is introduced: a reference in a position that wants a
/// case *is* one, and everywhere else it keeps meaning what it meant before —
/// the value, or the constructor function that `xs.map(Circle)` relies on.
pub fn ctor_ref_case(e: &ast::Expr) -> Option<(&str, &str)> {
    let qualified = match &e.kind {
        ast::ExprKind::Ident(n) => {
            // The shared wrapper is what a constructor used as a *value*
            // lowers to, so a `Case<V>` position reads the case back out of
            // its name exactly as it does out of the lambda a generic
            // variant's constructor still becomes.
            if let Some(rest) = n.strip_prefix(CTOR_WRAPPER_PREFIX) {
                return rest.split_once('$').map(|(v, c)| (c, v));
            }
            n.as_str()
        }
        ast::ExprKind::Lambda(params, body) => {
            let ast::ExprKind::Call(name, args, _) = &body.kind else {
                return None;
            };
            // Exactly the lambda `lower_ctor_refs` builds: one `__ctor{i}`
            // parameter per payload slot, passed straight through in order. A
            // hand-written `|x| Circle(x)` is deliberately *not* matched — it is
            // a function the author wrote, and reading a case out of it would be
            // guessing at intent.
            let shaped = params.len() == args.len()
                && params.iter().enumerate().all(|(i, p)| {
                    p.name == format!("{CTOR_PARAM_PREFIX}{i}")
                        && matches!(&args[i].kind, ast::ExprKind::Ident(a) if *a == p.name)
                });
            if !shaped {
                return None;
            }
            name.name()
        }
        _ => return None,
    };
    qualified.split_once('@')
}

pub mod lint;
