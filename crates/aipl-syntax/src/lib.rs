//! AIPL core types shared across every compiler crate: source spans, the
//! [`Error`] type, [`DebugOptions`] tracing, the [`ast`] tree, and the
//! AST-level `Type`/builtin helpers the parser, monomorphizer, codegen, and
//! loader all need. This crate has no external dependencies, so it forms the
//! fast-to-compile base of the workspace.

/// Byte-offset range in the source string.
pub type Span = std::ops::Range<usize>;

/// Smallest span covering both `a` and `b`.
pub fn join_spans(a: &Span, b: &Span) -> Span {
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

pub mod ast {
    use crate::Span;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Program {
        pub items: Vec<Item>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Item {
        Fn(Function),
        Struct(StructDecl),
        Variant(VariantDecl),
        Import(ImportDecl),
    }

    /// `import { foo, bar as baz } from "./util.aipl";` — a request to pull a
    /// specific set of items into the current file's namespace. The loader
    /// resolves `from` relative to the importing file.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ImportDecl {
        pub names: Vec<ImportName>,
        pub source: ImportSource,
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
        Path { path: String, span: Span },
        /// `from builtins` — the built-in namespace (`print`, `len`, …).
        /// Every builtin must be imported before use, exactly like a
        /// user item, so user idents never silently shadow them.
        Builtins { span: Span },
    }

    /// A constraint on a declared type variable, restricting which concrete
    /// types it may be instantiated with. `any` (the default almost every
    /// generic uses) accepts anything; `ord` narrows a variable to the
    /// primitives usable with `<`/`>` — currently the integers and `char` —
    /// so a signature like `minimum`/`maximum`'s can declare comparability
    /// itself instead of a caller hand-checking element types by name.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Bound {
        Any,
        Ord,
        Variant,
    }

    impl Bound {
        /// Parse a bound keyword from source text (the `any`/`ord` in
        /// `<T: any>`/`<T: ord>`), or `None` if unrecognized.
        pub fn from_name(name: &str) -> Option<Bound> {
            match name {
                "any" => Some(Bound::Any),
                "ord" => Some(Bound::Ord),
                "variant" => Some(Bound::Variant),
                _ => None,
            }
        }

        /// Source spelling of this bound, for error messages.
        pub fn name(&self) -> &'static str {
            match self {
                Bound::Any => "any",
                Bound::Ord => "ord",
                Bound::Variant => "variant",
            }
        }

        /// Does `ty` satisfy this bound? `Any` accepts everything; `Ord`
        /// accepts only the primitives usable with `<`/`>` — integers and
        /// `char`.
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
                Bound::Variant => matches!(ty, Type::Named(n) if is_variant(n)),
            }
        }
    }

    /// A declared generic type parameter: its name and bound (see [`Bound`]).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TypeParam {
        pub name: String,
        pub bound: Bound,
    }

    /// A function's shape apart from its body and source-only concerns (name,
    /// visibility, `.test`/`.doc`): its declared type variables, value
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
            Type::Unit
            | Type::Primitive(_)
            | Type::Named(_)
            // A variable `any` already normalized into is no longer an `any`.
            | Type::TypeVar(_)
            | Type::NoneInner
            | Type::EmptyArrayArg
            | Type::NoneLiteralArg
            | Type::ConcatStr => false,
            Type::Any => true,
            Type::Array(inner) | Type::Optional(inner) | Type::Set(inner) => ty_mentions_any(inner),
            Type::Dict(k, v) => ty_mentions_any(k) || ty_mentions_any(v),
            Type::Result(ok, err) => ty_mentions_any(ok) || ty_mentions_any(err),
            Type::Fn(params, ret) => params.iter().any(ty_mentions_any) || ty_mentions_any(ret),
            Type::Tuple(elems) => elems.iter().any(ty_mentions_any),
            Type::Generic(_, args) => args.iter().any(ty_mentions_any),
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Function {
        pub name: String,
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
        /// The text of an attached `.doc("...")` block, if any — structured
        /// documentation for the function, surfaced by the `doc` command and
        /// ignored by `run`/`build`/`check`. A `"""..."""` raw string is
        /// de-dented like any other (the parser's raw-string hook runs first).
        pub doc: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct StructDecl {
        pub name: String,
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
        pub name: String,
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
        pub name: String,
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
        pub name: String,
        /// Positional payload types; empty for a nullary case (e.g. `Empty`).
        pub payload: Vec<Type>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Param {
        pub name: String,
        pub ty: Type,
        /// `true` for a `mut self` receiver: the function mutates this
        /// parameter (only valid on the first parameter, named `self`). Such a
        /// function returns nothing and is called as `v.f(...)`.
        pub mutable: bool,
        /// `true` for a variadic ("zero or more") parameter written `T*`. The
        /// stored `ty` is the *sequence type* the body sees — `str` when the
        /// element `T` is `char`, otherwise `T[]` — and the element type is
        /// recoverable from it (`str` → `char`, `Array(e)` → `e`). At a call
        /// site such a parameter also accepts a single element `T` (wrapped to a
        /// one-item sequence) or an optional `T?` (empty/one-item sequence); the
        /// normalization to the sequence type happens in codegen. The body is
        /// unaffected — it just sees a plain `ty`.
        pub variadic: bool,
        /// `Some(expr)` for a *keyword* parameter, written `k: T = expr`: having
        /// a default is exactly what makes a parameter a keyword parameter.
        /// Keyword parameters must come after every positional parameter, may
        /// only be supplied by keyword at a call site (`f(1, k = 2)`), and are
        /// not part of the function's type. The loader expands every call so
        /// each omitted keyword argument is filled from this default; after
        /// loading, the default is inert (calls are fully positional).
        pub default: Option<Expr>,
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
    }

    pub fn is_unit(t: &Type) -> bool {
        matches!(t, Type::Unit)
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
        Set(Box<Type>),
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
        /// literal (`[]`), or empty set/dict literal (`#{}`/`#{:}`) — coerces to
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
        /// A `str` produced by `+`-concatenating two strings — distinguished
        /// from a plain `str` so codegen can specialize a lazy-concat
        /// representation for it (see [`crate::is_concat_str`]). Only meaningful as
        /// the type of a scalar value flowing to a `str` parameter; decays to
        /// a plain `str` once it's placed into any other container/context.
        ConcatStr,
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
        Set(Box<ConcreteType>),
        Dict(Box<ConcreteType>, Box<ConcreteType>),
        Result(Box<ConcreteType>, Box<ConcreteType>),
        Fn(Vec<ConcreteType>, Box<ConcreteType>),
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
                Type::TypeVar(_) | Type::Any | Type::Tuple(_) | Type::Generic(..) => return None,
                Type::Unit => ConcreteType::Unit,
                Type::Primitive(p) => ConcreteType::Primitive(*p),
                Type::Named(n) => ConcreteType::Named(n.clone()),
                Type::NoneInner => ConcreteType::NoneInner,
                Type::EmptyArrayArg => ConcreteType::EmptyArrayArg,
                Type::NoneLiteralArg => ConcreteType::NoneLiteralArg,
                Type::ConcatStr => ConcreteType::ConcatStr,
                Type::Optional(i) => ConcreteType::Optional(Box::new(i.to_concrete()?)),
                Type::Array(i) => ConcreteType::Array(Box::new(i.to_concrete()?)),
                Type::Set(i) => ConcreteType::Set(Box::new(i.to_concrete()?)),
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
                ConcreteType::NoneInner => Type::NoneInner,
                ConcreteType::EmptyArrayArg => Type::EmptyArrayArg,
                ConcreteType::NoneLiteralArg => Type::NoneLiteralArg,
                ConcreteType::ConcatStr => Type::ConcatStr,
                ConcreteType::Optional(i) => Type::Optional(Box::new(i.widen())),
                ConcreteType::Array(i) => Type::Array(Box::new(i.widen())),
                ConcreteType::Set(i) => Type::Set(Box::new(i.widen())),
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
        pub name: String,
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
        /// empty `[]`/`#{}`/`#{:}`, an `ok`/`err` whose other side is a
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
    }

    impl Expr {
        pub fn new(kind: ExprKind, span: Span) -> Self {
            Self {
                kind,
                span,
                ty: None,
            }
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
        Call(String, Vec<Expr>, bool),
        Binop(Box<Expr>, char, Box<Expr>),
        Neg(Box<Expr>),
        Not(Box<Expr>),
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
        For(String, Box<Expr>, Box<Expr>),
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
        /// element once. An empty `#{}` has element type `__none__` and coerces
        /// to any `T{}`, like an empty `[]`.
        SetLit(Vec<Expr>),
        /// `#{k0: v0, k1: v1, ...}` — a dict literal. Keys must share one
        /// scalar/`str` type and values one value type; duplicate keys keep the
        /// last binding (by value for scalars, by content for `str`). The empty
        /// dict is written `#{:}` (`#{}` is the empty set); like an empty `[]`
        /// its key/value types are `__none__` and coerce to any `#{K: V}`.
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
        KwArg(String, Box<Expr>),
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
        Ctor { name: String, bindings: Vec<String> },
        /// A string-literal pattern `"lit" => body` (matches a `str` scrutinee by
        /// content).
        Str(String),
        /// A char-literal pattern `'c' => body` (matches a `char` scrutinee by
        /// value). Like [`Pattern::Str`] the domain is open, so such a match
        /// must end in a `_` arm.
        Char(u8),
        /// An array/`str`-destructuring pattern `[e0, e1, ...] => body` (matches a
        /// `str` or array scrutinee by exact length, then, per element position,
        /// either a bound name or a literal-equality check). Each element is a
        /// bare identifier — a **binder** for the element at that position (typed
        /// `char` for a `str`, the element type for an array) — or a literal
        /// expression matched by equality (validated by the checker). An
        /// all-literal pattern (e.g. `[1, 2]`) still introduces no bindings.
        Array(Vec<Expr>),
        /// The wildcard / default arm `_ => body` (matches anything). Only valid
        /// for a `str` or array match, where it must be the last arm.
        Wildcard,
    }

    impl Pattern {
        /// The positional binders this pattern introduces, in order: a
        /// constructor pattern's payload binders, or an array/`str` pattern's
        /// identifier elements (its literal elements bind nothing). Empty for a
        /// string-literal or wildcard pattern.
        pub fn bindings(&self) -> Vec<String> {
            match self {
                Pattern::Ctor { bindings, .. } => bindings.clone(),
                Pattern::Array(elems) => elems
                    .iter()
                    .filter_map(|e| match &e.kind {
                        ExprKind::Ident(name) => Some(name.clone()),
                        _ => None,
                    })
                    .collect(),
                Pattern::Str(_) | Pattern::Char(_) | Pattern::Wildcard => Vec::new(),
            }
        }

        /// The constructor name for a `Ctor` pattern; `None` otherwise.
        pub fn ctor_name(&self) -> Option<&str> {
            match self {
                Pattern::Ctor { name, .. } => Some(name),
                Pattern::Str(_) | Pattern::Char(_) | Pattern::Array(_) | Pattern::Wildcard => None,
            }
        }
    }

    /// One arm of a `match`: a [`Pattern`] and its body.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MatchArm {
        pub pattern: Pattern,
        pub body: Expr,
        pub span: Span,
    }
}

use ast::{Primitive, Type};

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
        Type::Set(inner) => match &e.kind {
            K::SetLit(elems) if !elems.is_empty() => flex_all(elems.iter(), inner, target),
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
            K::Call(name, args, _) if name == "some" && args.len() == 1 => {
                Ok(flex_fit(&args[0], &Type::Unit, inner)?.map(|_| target.clone()))
            }
            _ => Ok(None),
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
const OPERATOR_BUILTINS: &[(&str, &str, &str)] = &[
    ("wrapping_add", "+", "__builtin_wrapping_add"),
    ("saturating_add", "+", "__builtin_saturating_add"),
    ("wrapping_sub", "-", "__builtin_wrapping_sub"),
    ("saturating_sub", "-", "__builtin_saturating_sub"),
    ("wrapping_mul", "*", "__builtin_wrapping_mul"),
    ("wrapping_increment", "++", "__builtin_wrapping_add"),
    ("saturating_increment", "++", "__builtin_saturating_add"),
    // The operators with a single semantics. They are listed here — rather than
    // being importable bare — so that *every* operator is imported the same way:
    // `name as op`. Reading an import list then tells you which operators a file
    // uses and, where it matters, which flavor, without the reader having to
    // know which operators happen to be ambiguous.
    //
    // Their canonical impl is the operator spelling itself: unlike `+`, there is
    // nothing to dispatch between, so the view maps straight through.
    ("equal", "==", "=="),
    ("not_equal", "!=", "!="),
    ("less_than", "<", "<"),
    ("greater_than", ">", ">"),
    ("less_than_or_equal", "<=", "<="),
    ("greater_than_or_equal", ">=", ">="),
    ("logical_and", "&&", "&&"),
    ("logical_or", "||", "||"),
    ("logical_not", "!", "!"),
    ("concat", "+++", "+++"),
    ("saturating_divide", "/", "/"),
    ("remainder", "%", "%"),
];

/// The named builtins that provide `op`, in [`OPERATOR_BUILTINS`] order.
///
/// Every operator has at least one, which is what lets the loader refuse a bare
/// operator import uniformly and name the alias to write instead.
pub fn operator_named_forms(op: &str) -> Vec<&'static str> {
    OPERATOR_BUILTINS
        .iter()
        .filter(|(_, o, _)| *o == op)
        .map(|(n, _, _)| *n)
        .collect()
}

/// If `name` is a named operator builtin, the `(operator, canonical impl)` it
/// provides — see [`OPERATOR_BUILTINS`].
pub fn operator_builtin(name: &str) -> Option<(&'static str, &'static str)> {
    OPERATOR_BUILTINS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, op, canonical)| (*op, *canonical))
}

/// The operator builtin providing `op` with the semantics of `canonical_impl` —
/// the inverse of [`operator_builtin`], for naming the import that pairs with
/// one a file already has (`__builtin_saturating_add` + `"++"` →
/// `saturating_increment`).
pub fn operator_builtin_named(op: &str, canonical_impl: &str) -> Option<&'static str> {
    OPERATOR_BUILTINS
        .iter()
        .find(|(_, o, c)| *o == op && *c == canonical_impl)
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

/// Spelling of a binary-operator char as stored in `ExprKind::Binop` (e.g. `'E'`
/// is `==`). Unary `Neg`/`Not` spell `-`/`!`.
pub fn binop_spelling(c: char) -> &'static str {
    match c {
        '+' => "+",
        '-' => "-",
        '*' => "*",
        '/' => "/",
        '%' => "%",
        '<' => "<",
        '>' => ">",
        'E' => "==",
        'N' => "!=",
        'L' => "<=",
        'G' => ">=",
        'A' => "&&",
        'O' => "||",
        // `++` — the increment operator (from `set n++;`). Lowered to `+` by the
        // loader after operator gating; this spelling is what the gate requires.
        'P' => "++",
        // `+++` — the string-concatenation operator.
        'C' => "+++",
        _ => "?",
    }
}

/// Collect the spellings of every operator used anywhere in `e` (for the
/// operator-import migration tooling).
pub fn collect_operators(e: &ast::Expr, out: &mut std::collections::HashSet<String>) {
    use ast::ExprKind as K;
    match &e.kind {
        // A shim's bindings are plain names; only its body holds expressions.
        K::Shim(_, _, body) => collect_operators(body, out),
        K::Binop(a, op, b) => {
            out.insert(binop_spelling(*op).to_string());
            collect_operators(a, out);
            collect_operators(b, out);
        }
        K::Neg(x) => {
            out.insert("-".to_string());
            collect_operators(x, out);
        }
        K::Not(x) => {
            out.insert("!".to_string());
            collect_operators(x, out);
        }
        K::Field(x, _) | K::Try(x) | K::Return(x) | K::KwArg(_, x) | K::Spread(x) => {
            collect_operators(x, out)
        }
        // An `Assign` LHS is a place (idents/fields only), so it can't
        // contain an operator — only the value and body need walking.
        K::Seq(a, b)
        | K::Index(a, b)
        | K::Let(_, _, a, b)
        | K::LetMut(_, _, a, b)
        | K::Assign(_, a, b)
        | K::For(_, a, b)
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
        K::Call(_, args, _) | K::ArrayLit(args) | K::SetLit(args) => {
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
        K::Lambda(_, body) => collect_operators(body, out),
        K::TupleLit(elems) => {
            for e in elems {
                collect_operators(e, out);
            }
        }
        K::Num(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::None | K::Unit | K::Ident(_) => {}
    }
}

/// The builtin error type. For now it's represented exactly like `str` (an
/// 8-byte heap pointer to a refcounted, NUL-terminated string) and behaves like
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

fn __builtin_print(self: str) !prints {}
// Split on each occurrence of `sep`, returning the parts (slices/views of `self`).
fn __builtin_split(self: str, sep: str) -> str[] { [] }
// Concatenate the parts with `sep` between consecutive elements.
// Flatten a sequence of sequences, placing `sep` between consecutive parts:
// `[[1,2],[3]].join(sep=0)` is `[1,2,0,3]`. Generic in the *element* type, so
// the receiver is one level deeper than it looks — for `T = char` it reads
// `str[] -> str` with a `char*` (i.e. `str`) separator, which is the string
// join. `sep` is variadic (a sequence, one element, or an optional one) and
// defaults to empty, so `xs.join()` concatenates; supplied, it is named.
fn __builtin_join<T: any>(self: T[][], sep: T* = []) -> T[] { [] }

// The file builtins return a Result; the `ok(..)` body coerces to the declared
// `..!Error` (codegen builds the real ok/err).
fn __builtin_read_file_to_string(self: str) !read_files -> str!Error { ok("") }
fn __builtin_write_string_to_file(self: str, contents: str) !write_files -> !Error { ok() }
// Every file at or below the directory `self`, recursively: each element is the
// path `self` joined with the entry's path beneath it (`"dir/sub/f.txt"`), in
// unspecified order. Directories are descended into, not listed; a symlink is a
// file (never followed). `err(..)` if the tree can't be walked at all — an
// unreadable directory, a non-UTF-8 name, or an entry whose kind the filesystem
// won't report.
fn __builtin_list_files(self: str) !list_files -> str[]!Error { ok([]) }

// Wall-clock nanoseconds since the Unix epoch (1970-01-01 UTC). Unsigned and
// 64-bit, so it counts up to the year 2554; a clock set before the epoch reads
// as 0. This is the system's *wall* clock, which can jump backwards (NTP,
// manual changes) — successive readings are not guaranteed to increase, so it
// measures "when", not reliably "how long".
fn __builtin_now_nanos() !clock -> u64 { 0 }
// Nanoseconds from the system's monotonic clock: it only ever counts up (no NTP
// step or manual clock change moves it), so a *difference* between two readings
// is a real elapsed duration. Its origin is unspecified — an absolute reading
// means nothing on its own, and nothing across processes or machines. Use this
// to measure "how long"; use `now_nanos` for "when".
fn __builtin_monotonic_now() !clock -> u64 { 0 }
// Spawn `self` with `args` (no shell involved) and wait for it to finish:
// `ok(ExecResult)` whenever it was actually launched, whatever it then exited
// with; `err(message)` only if it couldn't be launched at all (not found,
// permission denied, ...). `args` is a keyword parameter defaulting to no
// arguments, so `"true".execute_program()` runs a bare program and
// `"echo".execute_program(args = ["hi"])` passes argv.
fn __builtin_execute_program(self: str, args: str[] = []) !execute_program -> __builtin_ExecResult!Error {
    ok(__builtin_ExecResult { stdout: "", stderr: "", exit_code: 0 })
}

fn __builtin_to_str<T: any>(self: T) -> str { "" }
// Structural hash, consistent with `==`.
fn __builtin_hash<T: any>(self: T) -> i64 { 0 }
fn __builtin_trim(self: str) -> str { self }
// Concatenate `self` with itself `n` times; returns `""` for `n == 0`. The count
// is unsigned, matching `len`: a repeat count is never negative, and its usual
// source is length/column arithmetic (`" ".repeat(indent)`).
fn __builtin_repeat(self: str, n: u64) -> str { "" }
// True if every byte is ASCII whitespace (or the string is empty).
fn __builtin_is_all_whitespace(self: str) -> bool { false }
// True if `self` begins / ends with the argument — `str` bytes or `T[]`
// elements (the empty pattern always matches). A str receiver is dispatched in
// the checker / codegen (the `T[]` signature doesn't unify with `str`).
fn __builtin_starts_with<T: any>(self: T[], prefix: T[]) -> bool { false }
fn __builtin_ends_with<T: any>(self: T[], suffix: T[]) -> bool { false }
// `self[at..].starts_with(prefix)` without building the slice: the same answer,
// comparing in place from `at`. `at` clamps like a slice bound, so an `at` past
// the end matches only the empty pattern. The pattern is variadic exactly as
// `starts_with`'s, and dispatched with it in the checker / codegen. Written by
// the compiler's operation-fusion pass; also callable directly.
fn __builtin_starts_with_at<T: any>(self: T[], prefix: T[], at: u64) -> bool { false }
// True if `self` contains the needle: a `T[]` (or `str`) needle matches as a
// contiguous subsequence (substring), a `T` (or `char`) as a single element,
// and a `T?` as its element when `some` — a `none` needle is nothing to find,
// so it's `false` (unlike `starts_with`/`ends_with`, whose `none` pattern is
// the empty pattern and matches). A str receiver is dispatched in the
// checker / codegen (the `T[]` signature doesn't unify with `str`).
fn __builtin_contains<T: any>(self: T[], needle: T[]) -> bool { false }
// Smaller / larger of two comparable values (codegen compares and selects).
// `ord` is the same bound `minimum`/`maximum` carry — integers, `char` and
// `str` — so these work at any integer width rather than `i64` alone.
fn __builtin_min<T: ord>(self: T, other: T) -> T { self }
fn __builtin_max<T: ord>(self: T, other: T) -> T { self }
// Smallest / largest element of an array, or `none` if empty (codegen folds
// over the elements). `ord` restricts `T` to comparable elements (integer or
// char), enforced generically by the checker's bound-checking, not a special
// case for these two names.
fn __builtin_minimum<T: ord>(self: T[]) -> T? { none }
fn __builtin_maximum<T: ord>(self: T[]) -> T? { none }
// Element/byte count. Unsigned: a length is never negative, so `u64` keeps
// `len`-derived arithmetic (`a.len() - b.len()`, capacity math) in the
// saturating-at-zero unsigned world rather than silently going negative.
// Index/slice bounds accept either signedness, so `xs[xs.len() - 1]` still
// works without a conversion.
fn __builtin_len<T: any>(self: T[]) -> u64 { 0 }
// Whether the receiver holds anything at all — the same question
// `len(self) != 0` asks, without routing an answer that is already a `bool`
// through a count and a comparison. Takes every receiver `len` does (array,
// `str`, set, dict), dispatched the same way in the checker and codegen; the
// `len_gt_zero` lint points every `0 < x.len()` here.
fn __builtin_is_nonempty<T: any>(self: T[]) -> bool { false }
fn __builtin_is_some<T: any>(self: T?) -> bool { false }
// Character classification: ASCII whitespace (space/tab/newline/carriage return).
fn __builtin_is_space(self: char) -> bool { false }
// Character classification: ASCII decimal digit ('0' through '9').
fn __builtin_is_digit(self: char) -> bool { false }
// ASCII decimal digit ('0'..'9') to its 0..9 value, `none` for any other char.
fn __builtin_to_digit(self: char) -> i64? { none }

// Set ops: membership and union.
fn __builtin_has<T: any>(self: #{T}, x: T) -> bool { false }
fn __builtin_union<T: any>(self: #{T}, other: #{T}) -> #{T} { self }

// Dict ops: lookup (none if absent) and membership.
fn __builtin_get<K: any, V: any>(self: #{K: V}, key: K) -> V? { none }
fn __builtin_contains_key<K: any, V: any>(self: #{K: V}, key: K) -> bool { false }

fn __builtin_map<T: any, U: any>(self: T[], f: (T) -> U) -> U[] { [] }
fn __builtin_filter<T: any>(self: T[], pred: (T) -> bool) -> T[] { self }
// `self.filter(keep).map(f)` in one pass over the elements, instead of one pass
// each with an array between them. Written by the compiler's operation-fusion
// pass; also callable directly, and worth writing by hand when the two halves
// belong together.
fn __builtin_filter_map<T: any, U: any>(self: T[], keep: (T) -> bool, f: (T) -> U) -> U[] { [] }
fn __builtin_intersperse<T: any>(self: T[], sep: T) -> T[] { self }
fn __builtin_same_case<T: variant>(self: T, other: T) -> bool { false }
fn __builtin_count_is_less_than<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_at_most<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_greater_than<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_at_least<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_equal<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_count_is_not_equal<T: any>(self: T[], x: T, limit: u64) -> bool { false }
fn __builtin_first<T: any>(self: T[]) -> T? { none }
fn __builtin_last<T: any>(self: T[]) -> T? { none }
fn __builtin_drop_first<T: any>(self: T[]) -> T[] { self }
fn __builtin_drop_last<T: any>(self: T[]) -> T[] { self }
fn __builtin_drop_n<T: any>(self: T[], n: u64) -> T[] { self }
fn __builtin_drop_last_n<T: any>(self: T[], n: u64) -> T[] { self }
// NOTE: `all`, `count_while`, `is_some_and`, `int_parse`, `trim_while`,
// `try_map`, `value_or`, and `value_or_err` are
// *not* declared here — they're implemented in AIPL (`aipl-mono/src/builtin_*.aipl`),
// which is the single source of both their body and their signature.
// `aipl_mono::aipl_builtin_sig_decls()` feeds those signatures to the checker and
// codegen; see `AIPL_BUILTIN_SOURCES` in aipl-mono.
fn __builtin_zip_with<T: any, U: any, V: any>(self: T[], other: U[], f: (T, U) -> V) -> V[] { [] }
fn __builtin_push<T: any>(mut self: T[], x: T) {}
// Append every element of `other` to `self` — `push` for a whole array. A
// builtin rather than an AIPL loop over `push` so it can size the destination
// once: growing by `other.len()` in a single reserve turns N reallocations into
// at most one, and the elements move as one `memcpy` plus one retain pass.
fn __builtin_extend<T: any>(mut self: T[], other: T[]) {}
// Reverse the elements of an array or the bytes of a string.
fn __builtin_reverse<T: any>(self: T[]) -> T[] { [] }
// Ascending sort. `ord` restricts `T` to comparable elements (integer, char, or
// str), enforced generically by the checker's bound-checking.
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
// Internal: emitted by the compiler for array-literal spreads (`[..xs, y]`).
// `reserve` sizes the accumulator for the whole literal up front and makes it
// uniquely owned; `append`/`concat` then write into that reserved capacity in
// place. Named `__aipl_*` (not `__builtin_*`) so no import can name them.
fn __aipl_arr_reserve<T: any>(self: T[], extra: u64) -> T[] { self }
fn __aipl_arr_append<T: any>(self: T[], x: T) -> T[] { self }
fn __aipl_arr_concat<T: any>(self: T[], other: T[]) -> T[] { self }
// Internal: emitted by the compiler for template-literal concatenation.
fn __aipl_concat(a: str, b: str) -> str { "" }
// Internal: emitted for each interpolation in a template literal.
// Passes a `str` through unchanged; converts any other type via `to_str`.
fn __template_interp<T: any>(self: T) -> str { "" }
"#;

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
            ConcreteType::Named(s) => s.clone(),
            ConcreteType::Optional(inner) => format!("{}?", type_name(inner)),
            ConcreteType::Array(inner) => format!("{}[]", type_name(inner)),
            ConcreteType::Set(inner) => format!("#{{{}}}", type_name(inner)),
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

pub fn type_name(t: &Type) -> String {
    match t {
        Type::Unit => "()".into(),
        Type::Primitive(p) => p.name().into(),
        Type::Named(s) => s.clone(),
        // A variable renders as the parameter the user wrote; the anonymous one
        // an `any` normalized to has no name to show, so it renders as `any`.
        Type::TypeVar(v) if v.is_empty() => "any".into(),
        Type::TypeVar(v) => v.clone(),
        Type::Optional(inner) => format!("{}?", type_name(inner)),
        Type::Array(inner) => format!("{}[]", type_name(inner)),
        Type::Set(inner) => format!("#{{{}}}", type_name(inner)),
        Type::Dict(k, v) => format!("#{{{}: {}}}", type_name(k), type_name(v)),
        Type::Result(ok, err) => format!("{}!{}", type_name(ok), type_name(err)),
        Type::Fn(params, ret) => {
            let ps = params.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("({ps}) -> {}", type_name(ret))
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

/// Valid array element types: the 8-byte value types — every integer width
/// (`i8`..`i64`, `u8`..`u64`, each stored canonicalized in an 8-byte slot like
/// `i64`), `bool`, `char`, `str`, and (nested) arrays, which are themselves
/// 8-byte heap pointers. Structs and optionals are inline composites wider than
/// 8 bytes and aren't yet supported as elements.
pub fn is_array_elem(t: &Type) -> bool {
    is_int_ty(t)
        || matches!(
            t,
            Type::Primitive(Primitive::Bool | Primitive::Char | Primitive::Str) | Type::Array(_)
        )
}

/// Valid set element types: the scalar value types — every integer width
/// (`i8`..`i64`, `u8`..`u64`), `bool`, `char`, and `str`. Scalars compare by
/// value; `str` compares by content (see the set runtime). Nested containers
/// (arrays/sets/optionals/structs) are not yet supported as set elements.
///
/// Also the "storable scalar" test behind struct fields and variant payloads,
/// which hold exactly these inline.
pub fn is_set_elem(t: &Type) -> bool {
    is_int_ty(t)
        || matches!(
            t,
            Type::Primitive(Primitive::Bool | Primitive::Char | Primitive::Str)
        )
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

/// Built-in idents that must be brought into scope with
/// `import { .. } from builtins;` before use. These are the by-name
/// callable builtins; `some`/`none`/`match` and operators (`+`, `==`)
/// are language syntax, not importable idents.
pub const IMPORTABLE_BUILTINS: &[&str] = &[
    "print",
    "split",
    "join",
    "intersperse",
    "same_case",
    "count_is_less_than",
    "count_is_at_most",
    "count_is_greater_than",
    "count_is_at_least",
    "count_is_equal",
    "count_is_not_equal",
    "first",
    "last",
    "drop_first",
    "drop_last",
    "drop_n",
    "drop_last_n",
    "to_str",
    "map",
    "try_map",
    "filter",
    "filter_map",
    "all",
    "any",
    "left_fold",
    "right_fold",
    "opt_left_fold",
    "opt_right_fold",
    "zip_with",
    "trim",
    "is_all_whitespace",
    "starts_with",
    "starts_with_at",
    "ends_with",
    "len",
    "is_nonempty",
    "push",
    "extend",
    "is_some",
    "is_some_and",
    "is_err_and",
    "map_err",
    "map_ok",
    "int_parse",
    "is_space",
    "is_digit",
    "to_digit",
    "trim_while",
    "count",
    "count_while",
    "value_or",
    "value_or_err",
    "contains",
    "has",
    "read_file_to_string",
    "write_string_to_file",
    "list_files",
    "now_nanos",
    "monotonic_now",
    "execute_program",
    "union",
    "get",
    "contains_key",
    "hash",
    "min",
    "max",
    "minimum",
    "maximum",
    "reverse",
    "sort",
    "sort_by",
    "repeat",
];

/// Canonical internal name for an importable builtin, or `None` if `name`
/// isn't one. The loader rewrites imported builtin references to this
/// reserved name (which users can't write directly), so a user ident can
/// never collide with — or silently shadow — a builtin.
pub fn builtin_canonical(name: &str) -> Option<String> {
    if IMPORTABLE_BUILTINS.contains(&name) {
        Some(format!("__builtin_{name}"))
    } else {
        None
    }
}

/// The *operations* of each shimmable effect: the builtins through which that
/// effect actually reaches its underlying resource. A `shim <effect> { .. }`
/// block must bind every one of them — a partial shim would let the body reach
/// the real resource while the enclosing function stops declaring the effect,
/// which would make that declaration a lie.
///
/// This is the one table that decides what can be shimmed. Every other part of
/// the feature (parser, checker, codegen, runtime slots) reads it rather than
/// naming a specific effect, so opening up another effect is an entry here plus
/// the runtime dispatch for its operations. Only `clock` is listed for now.
pub const SHIMMABLE_EFFECTS: &[(&str, &[&str])] = &[("clock", &["now_nanos", "monotonic_now"])];

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
pub const SHIM_SLOT_COUNT: usize = 2;

/// Built-in *type* names that must be brought into scope with
/// `import { .. } from builtins;` before use — the type-level analog of
/// [`IMPORTABLE_BUILTINS`]. Unlike the ambient builtin `Error` type (which
/// needs no import), these behave like any other importable builtin: gated,
/// and mapped to a reserved canonical name so a user's own type of the same
/// name can never silently collide.
pub const IMPORTABLE_BUILTIN_TYPES: &[&str] = &["Span", "ExecResult"];

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
/// keyword-parameter / struct-field default expressions — pre-order.
pub fn each_expr(program: &ast::Program, f: &mut impl FnMut(&ast::Expr)) {
    for item in &program.items {
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
            ast::Item::Variant(_) | ast::Item::Import(_) => {}
        }
    }
}

/// Visit `e` and every expression nested inside it, pre-order.
/// Rewrite every mention of a declaration's own type parameters from
/// [`ast::Type::Named`] to [`ast::Type::TypeVar`], throughout its signature,
/// its field/payload types, and any annotation inside its body.
///
/// Run once at the end of parsing, which is the earliest point the distinction
/// is knowable and the last point at which *every* path agrees on it: source
/// files reach the checker through the loader, but the builtin signatures are
/// parsed directly (`native_builtin_decls`), and a promotion living in the
/// loader would leave those spelling `T` as an ordinary name. Doing it here
/// means no later pass has to ask "is this `Named` really a variable?" — the
/// question the string-sentinel encodings existed to answer, each differently.
///
/// A type parameter shadows a struct of the same name, which is what the
/// previous name-keyed substitutions effectively assumed and could not enforce.
pub fn promote_type_vars(program: &mut ast::Program) {
    for item in &mut program.items {
        match item {
            ast::Item::Fn(f) => {
                let vars: Vec<String> = f.sig.type_var_names();
                for p in &mut f.sig.params {
                    promote_ty(&mut p.ty, &vars);
                }
                if let Some(r) = &mut f.sig.return_ty {
                    promote_ty(r, &vars);
                }
                promote_in_expr(&mut f.body, &vars);
                if let Some(t) = &mut f.test_body {
                    promote_in_expr(t, &vars);
                }
            }
            ast::Item::Struct(s) => {
                let vars: Vec<String> = s.type_vars.iter().map(|v| v.name.clone()).collect();
                for fd in &mut s.fields {
                    promote_ty(&mut fd.ty, &vars);
                    if let Some(d) = &mut fd.default {
                        promote_in_expr(d, &vars);
                    }
                }
            }
            ast::Item::Variant(v) => {
                let vars: Vec<String> = v.type_vars.iter().map(|t| t.name.clone()).collect();
                for case in &mut v.cases {
                    for ty in &mut case.payload {
                        promote_ty(ty, &vars);
                    }
                }
            }
            ast::Item::Import(_) => {}
        }
    }
}

/// [`promote_type_vars`] for one type: every `Named` whose name is one of
/// `vars` becomes a `TypeVar`, recursively.
fn promote_ty(ty: &mut ast::Type, vars: &[String]) {
    use ast::Type as T;
    match ty {
        T::Named(n) if vars.iter().any(|v| v == n) => *ty = T::TypeVar(n.clone()),
        T::Unit
        | T::Primitive(_)
        | T::Named(_)
        | T::TypeVar(_)
        | T::Any
        | T::NoneInner
        | T::EmptyArrayArg
        | T::NoneLiteralArg
        | T::ConcatStr => {}
        T::Optional(i) | T::Array(i) | T::Set(i) => promote_ty(i, vars),
        T::Dict(a, b) | T::Result(a, b) => {
            promote_ty(a, vars);
            promote_ty(b, vars);
        }
        T::Tuple(es) | T::Generic(_, es) => {
            for e in es {
                promote_ty(e, vars);
            }
        }
        T::Fn(ps, r) => {
            for p in ps {
                promote_ty(p, vars);
            }
            promote_ty(r, vars);
        }
    }
}

/// [`promote_type_vars`] for the annotations inside a body — `let x: T[] = ..`,
/// `mut n: T = ..`, and a lambda parameter's declared type.
fn promote_in_expr(e: &mut ast::Expr, vars: &[String]) {
    use ast::ExprKind as K;
    match &mut e.kind {
        K::Let(_, ty, _, _) | K::LetMut(_, ty, _, _) => {
            if let Some(t) = ty {
                promote_ty(t, vars);
            }
        }
        K::Lambda(params, _) => {
            for p in params {
                if let Some(t) = &mut p.ty {
                    promote_ty(t, vars);
                }
            }
        }
        _ => {}
    }
    for child in each_subexpr_mut(e) {
        promote_in_expr(child, vars);
    }
}

/// The direct sub-expressions of `e`, mutably — the shape [`each_subexpr`]
/// walks, for passes that rewrite rather than inspect.
fn each_subexpr_mut(e: &mut ast::Expr) -> Vec<&mut ast::Expr> {
    use ast::ExprKind as K;
    match &mut e.kind {
        K::Num(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::Ident(_) | K::None | K::Unit => {
            vec![]
        }
        K::Shim(_, _, body) => vec![body.as_mut()],
        K::Call(_, args, _) | K::ArrayLit(args) | K::SetLit(args) | K::TupleLit(args) => {
            args.iter_mut().collect()
        }
        K::Construct(_, inits) => inits.iter_mut().map(|i| &mut i.value).collect(),
        K::DictLit(pairs) => pairs.iter_mut().flat_map(|(k, v)| [k, v]).collect(),
        K::Binop(a, _, b)
        | K::Seq(a, b)
        | K::Index(a, b)
        | K::Let(_, _, a, b)
        | K::LetMut(_, _, a, b)
        | K::For(_, a, b)
        | K::While(a, b) => vec![a.as_mut(), b.as_mut()],
        K::Assign(a, b, c) | K::If(a, b, c) => vec![a.as_mut(), b.as_mut(), c.as_mut()],
        K::Neg(x)
        | K::Not(x)
        | K::Field(x, _)
        | K::Try(x)
        | K::Return(x)
        | K::KwArg(_, x)
        | K::Spread(x)
        | K::Lambda(_, x) => vec![x.as_mut()],
        K::Match(scrutinee, arms) => {
            let mut v = vec![scrutinee.as_mut()];
            v.extend(arms.iter_mut().map(|a| &mut a.body));
            v
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

pub fn each_subexpr(e: &ast::Expr, f: &mut impl FnMut(&ast::Expr)) {
    use ast::ExprKind as K;
    f(e);
    match &e.kind {
        K::Num(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::Ident(_) | K::None | K::Unit => {}
        // A shim's bindings name functions; only its body nests expressions.
        K::Shim(_, _, body) => each_subexpr(body, f),
        K::Call(_, args, _) | K::ArrayLit(args) | K::SetLit(args) | K::TupleLit(args) => {
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
        K::Binop(a, _, b)
        | K::Seq(a, b)
        | K::Index(a, b)
        | K::Let(_, _, a, b)
        | K::LetMut(_, _, a, b)
        | K::For(_, a, b)
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
        | K::Not(x)
        | K::Field(x, _)
        | K::Try(x)
        | K::Return(x)
        | K::KwArg(_, x)
        | K::Spread(x)
        | K::Lambda(_, x) => each_subexpr(x, f),
        K::Match(scrutinee, arms) => {
            each_subexpr(scrutinee, f);
            for arm in arms {
                each_subexpr(&arm.body, f);
            }
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

/// Lints: *squelchable* errors. AIPL has no warnings — every diagnostic is an
/// error and fails the compile — but the errors this module produces (and only
/// these) can be squelched by appending `#[allow]` to the offending line. The
/// marker is line-scoped: it silences every lint whose reported span starts on
/// its line, and nothing else. Regular errors (type mismatches, unknown names,
/// parse errors, ...) take no notice of `#[allow]`.
///
/// A lint flags code that is *legal but has a clearly better spelling*; its
/// message must name that better spelling. The loader runs `aipl_mono::check` on every
/// file right after parsing (the markers come from the lexer via
/// `parse_with_allows`), so lints fire before type checking.
/// Prefix of the binding the loader synthesizes for a `..base` struct spread
/// (`aipl-loader`'s `desugar_struct_spread`). It is the one thing that ties the
/// two ends of that desugaring together: the loader emits the binding carrying
/// the target struct's type, and the checker recognizes it so a failed
/// annotation is reported as the spread the user actually wrote rather than as
/// an internal `let`. Shared rather than spelled twice, because a silent
/// mismatch would turn a good diagnostic back into a confusing one.
pub const SPREAD_BASE_PREFIX: &str = "__spread_base$";

pub mod lint {
    use super::ast::{Expr, ExprKind, ImportSource, Item, Pattern, Program, Type};
    use super::{each_expr, Error, Span};
    use std::collections::HashSet;

    /// Run every lint over `program` — function bodies, `.test` blocks, and
    /// keyword-parameter / struct-field default expressions — then drop the
    /// hits squelched by a same-line `#[allow]` (`allows` are the marker spans
    /// the lexer collected). Returns every surviving lint error.
    pub fn check(program: &Program, src: &str, allows: &[Span]) -> Result<(), Vec<Error>> {
        let allowed: std::collections::HashSet<usize> =
            allows.iter().map(|sp| line_of(src, sp.start)).collect();
        let mut hits: Vec<Error> = Vec::new();
        // The three slice lints partition the range forms rather than overlap:
        // a whole-receiver range belongs to `slice_whole` and the other two bow
        // out of it explicitly, so `x[0..x.len()]` yields one hit advising `x`
        // instead of a chain of rewrites through `x[0..]` or `x[..x.len()]`.
        // Order between them therefore carries no meaning.
        each_expr(program, &mut |e| slice_whole(e, src, &mut hits));
        each_expr(program, &mut |e| slice_to_len(e, src, &mut hits));
        each_expr(program, &mut |e| slice_from_zero(e, src, &mut hits));
        each_expr(program, &mut |e| eta_lambda(e, &mut hits));
        each_expr(program, &mut |e| match_is_some_and(e, &mut hits));
        each_expr(program, &mut |e| match_value_or(e, src, &mut hits));
        each_expr(program, &mut |e| match_map_err(e, &mut hits));
        each_expr(program, &mut |e| match_map_ok(e, &mut hits));
        each_expr(program, &mut |e| field_init_shorthand(e, src, &mut hits));
        // Only where this file's `push` is the builtin — see `pipeline_names`,
        // which also reports what `map`/`filter` are called here.
        let pipeline = pipeline_names(program);
        each_expr(program, &mut |e| {
            push_loop_pipeline(e, src, &pipeline, &mut hits)
        });
        // Only where a `++` flavor provably matches this file's `+` — see
        // `matching_increment`, which also names the import when it's missing.
        if let Some(incr) = matching_increment(program) {
            each_expr(program, &mut |e| incr_by_one(e, incr, &mut hits));
        }
        // Only for the comparisons this file's `<`/`>` really are — see
        // `len_zero_cmp`, which also says whether `is_nonempty` needs importing.
        let cmp = len_zero_cmp(program);
        each_expr(program, &mut |e| len_gt_zero(e, src, &cmp, &mut hits));
        unused_imports(program, &mut hits);
        fn_body_type_stutter(program, src, &mut hits);
        hits.retain(|e| match &e.span {
            Some(sp) => !allowed.contains(&line_of(src, sp.start)),
            None => true,
        });
        // Every surviving hit, not just the first: lints are independent
        // findings over the whole file, so there is nothing to recover from and
        // no reason to make the reader re-run to see the next one.
        if hits.is_empty() {
            Ok(())
        } else {
            Err(hits)
        }
    }

    /// 0-based line number of byte offset `pos` in `src`.
    fn line_of(src: &str, pos: usize) -> usize {
        src[..pos.min(src.len())].matches('\n').count()
    }

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
    fn fn_body_type_stutter(program: &Program, src: &str, hits: &mut Vec<Error>) {
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

    /// An imported name nothing in the file references. Legal — an unused import
    /// changes no behavior — but it misstates what the file depends on and
    /// outlives whatever once used it; the better spelling is to drop the name.
    ///
    /// "Referenced" is deliberately syntactic and *generous*: any expression,
    /// pattern, operator, or type position mentioning the local name counts,
    /// including one a local definition or type parameter shadows. That is the
    /// safe direction — a lint is a hard error here, so a missed dead import
    /// costs one stale line, while a false hit costs a build that cannot be made
    /// to pass except by `#[allow]`.
    fn unused_imports(program: &Program, hits: &mut Vec<Error>) {
        let used = referenced_names(program);
        for item in &program.items {
            let Item::Import(decl) = item else {
                continue;
            };
            for n in &decl.names {
                if used.contains(n.local()) {
                    continue;
                }
                // The bare `-`/`<`/`>`/`||`/`!` operator tokens carry no span of
                // their own (see the parser's `op_import`), so point at the
                // import statement instead — an empty span would put the caret
                // on the file's first line, nowhere near the import.
                let span = if n.span.is_empty() {
                    match &decl.source {
                        ImportSource::Builtins { span } => span.clone(),
                        ImportSource::Path { span, .. } => span.clone(),
                    }
                } else {
                    n.span.clone()
                };
                hits.push(Error::at(
                    format!(
                        "unused import {:?}: nothing in this file references it; drop it from the \
                         import list",
                        n.local()
                    ),
                    span,
                ));
            }
        }
    }

    /// Every name the file mentions outside its own import lists: called and
    /// referenced idents, constructed structs, match constructors, operator
    /// spellings, and every name appearing in a type — signatures, struct
    /// fields, variant payloads, and inline `let`/lambda annotations.
    fn referenced_names(program: &Program) -> HashSet<String> {
        let mut out = HashSet::new();
        // Type positions that belong to the item rather than to any expression.
        for item in &program.items {
            match item {
                Item::Fn(f) => {
                    for p in &f.sig.params {
                        collect_ty_names(&p.ty, &mut out);
                    }
                    if let Some(t) = &f.sig.return_ty {
                        collect_ty_names(t, &mut out);
                    }
                }
                Item::Struct(s) => {
                    for field in &s.fields {
                        collect_ty_names(&field.ty, &mut out);
                    }
                }
                Item::Variant(v) => {
                    for case in &v.cases {
                        for t in &case.payload {
                            collect_ty_names(t, &mut out);
                        }
                    }
                }
                Item::Import(_) => {}
            }
        }
        // `each_expr` already reaches every nested expression, so each node only
        // needs inspecting at its own level.
        each_expr(program, &mut |e| match &e.kind {
            ExprKind::Ident(n) | ExprKind::Call(n, _, _) | ExprKind::Construct(n, _) => {
                out.insert(n.clone());
            }
            ExprKind::Binop(_, op, _) => {
                out.insert(super::binop_spelling(*op).to_string());
            }
            ExprKind::Neg(_) => {
                out.insert("-".to_string());
            }
            ExprKind::Not(_) => {
                out.insert("!".to_string());
            }
            // A shim's name and bindings all name functions.
            ExprKind::Shim(name, binds, _) => {
                out.insert(name.clone());
                for (a, b) in binds {
                    out.insert(a.clone());
                    out.insert(b.clone());
                }
            }
            ExprKind::Let(_, Some(t), _, _) | ExprKind::LetMut(_, Some(t), _, _) => {
                collect_ty_names(t, &mut out);
            }
            ExprKind::Lambda(params, _) => {
                for p in params {
                    if let Some(t) = &p.ty {
                        collect_ty_names(t, &mut out);
                    }
                }
            }
            ExprKind::Match(_, arms) => {
                for arm in arms {
                    if let Pattern::Ctor { name, .. } = &arm.pattern {
                        // A `V.A` path names the variant as well as the case.
                        if let Some((v, _)) = name.split_once('.') {
                            out.insert(v.to_string());
                        }
                        out.insert(name.clone());
                    }
                }
            }
            _ => {}
        });
        out
    }

    /// Every name `ty` mentions, so an import used only in a type position still
    /// counts as used. Variants are spelled out rather than wildcarded so a new
    /// `Type` forces a decision here.
    fn collect_ty_names(ty: &Type, out: &mut HashSet<String>) {
        match ty {
            Type::Named(n) => {
                out.insert(n.clone());
            }
            // A type parameter is bound by the signature, not imported.
            Type::TypeVar(_) => {}
            Type::Generic(base, args) => {
                out.insert(base.clone());
                for a in args {
                    collect_ty_names(a, out);
                }
            }
            Type::Optional(inner) | Type::Array(inner) | Type::Set(inner) => {
                collect_ty_names(inner, out)
            }
            Type::Dict(k, v) | Type::Result(k, v) => {
                collect_ty_names(k, out);
                collect_ty_names(v, out);
            }
            Type::Fn(params, ret) => {
                for p in params {
                    collect_ty_names(p, out);
                }
                collect_ty_names(ret, out);
            }
            Type::Tuple(ts) => {
                for t in ts {
                    collect_ty_names(t, out);
                }
            }
            Type::Unit
            | Type::Primitive(_)
            | Type::Any
            | Type::NoneInner
            | Type::EmptyArrayArg
            | Type::NoneLiteralArg
            | Type::ConcatStr => {}
        }
    }

    /// True when `end` is exactly `len(obj)` — the receiver's own whole length,
    /// however it was spelled (`x.len()` is stored as the free call `len(x)`).
    /// Shared by the three slice lints, which divide the range forms between
    /// them and so must agree on what "runs to the end" means.
    fn end_is_receiver_len(end: &Expr, obj: &Expr) -> bool {
        let ExprKind::Call(name, args, _) = &end.kind else {
            return false;
        };
        name == "len" && args.len() == 1 && args[0] == *obj
    }

    /// `x[0..]` / `x[..x.len()]` / `x[0..x.len()]` — a slice whose range covers
    /// the receiver end to end, which is what `x` already is. The slice is not
    /// free: an array slice allocates a fresh block and copies every element
    /// (`aipl_arr_slice`), so this is a redundant copy as well as a redundant
    /// spelling. Dropping it is safe because AIPL arrays have value semantics —
    /// mutating an aliased binding copies first (see
    /// `cases/arrays/push_aliased_array`), so `y = x` cannot be surprised by a
    /// later `set x[..]` the way a shared reference could.
    ///
    /// This is the whole-range corner of the three slice lints, and it takes
    /// priority over both: [`slice_to_len`] would advise `x[0..]` and
    /// [`slice_from_zero`] `x[..x.len()]`, each of which is still this shape and
    /// would need a second round trip. Both bow out when this one applies, so a
    /// full-range slice produces exactly one hit whichever way it was written.
    ///
    /// A start of `0` counts however it was written: `x[..y]` synthesizes a zero
    /// with an empty span, and unlike its siblings this lint has no reason to
    /// tell that from a written `0` — both cover the whole receiver, and the
    /// advice names neither bound.
    fn slice_whole(e: &Expr, src: &str, hits: &mut Vec<Error>) {
        let ExprKind::Slice(obj, start, end) = &e.kind else {
            return;
        };
        if !matches!(start.kind, ExprKind::Num(0)) {
            return;
        }
        // An absent end already runs to the length; a present one has to say so.
        if let Some(end) = end {
            if !end_is_receiver_len(end, obj) {
                return;
            }
        }
        // Quote the receiver only when its span really ends where the `[`
        // begins. A call-shaped span stops before its parens (the trap
        // [`slice_from_zero`] documents for the end bound), so splicing
        // `f(x)[0..]` would advise the bare `f`. Checking the source rather than
        // the expression kind keeps every complete receiver quotable —
        // `x.field`, `x.0` and `xs[i]` all name themselves fine.
        let quotable = src[obj.span.end..].trim_start().starts_with('[');
        let use_it = if quotable {
            format!("use \"{}\" itself", &src[obj.span.clone()])
        } else {
            "use the receiver itself".to_string()
        };
        // Point at a bound rather than the whole expression: `#[allow]` is
        // line-scoped (see `allow_squelch`), and a receiver may span lines, so
        // the expression's own span could start on a line that can't carry the
        // marker. A bound sits inside the brackets, always on the line the
        // slice is written on. The written `0` is the natural target; when it
        // was elided (`x[..x.len()]`) its span is empty, so the end bound —
        // which must be present in that form — stands in.
        let span = if start.span.is_empty() {
            end.as_ref()
                .expect("elided start implies an end bound")
                .span
                .clone()
        } else {
            start.span.clone()
        };
        hits.push(Error::at(
            format!(
                "slice covers the whole receiver — {use_it} \
                 (or append #[allow] to this line to keep it)"
            ),
            span,
        ));
    }

    /// `x[y..x.len()]` — the end bound is the receiver's own length, which is
    /// what the open-ended form already means; recommend `x[y..]`. Purely
    /// syntactic: the receiver and the `len` argument must be the same
    /// expression (spans ignored), so aliases or computed receivers that merely
    /// happen to be equal at runtime are not flagged.
    fn slice_to_len(e: &Expr, src: &str, hits: &mut Vec<Error>) {
        let ExprKind::Slice(obj, start, Some(end)) = &e.kind else {
            return;
        };
        if !end_is_receiver_len(end, obj) {
            return;
        }
        // A zero start makes this the *whole* receiver, which is
        // [`slice_whole`]'s to report — and its advice (`x`) is better than the
        // `x[0..]` this one would give. The two are mutually exclusive, so
        // neither has to run before the other.
        if matches!(start.kind, ExprKind::Num(0)) {
            return;
        }
        let recv = &src[obj.span.clone()];
        let st = &src[start.span.clone()];
        hits.push(Error::at(
            format!(
                "slice end is the receiver's whole length — use the open-ended \
                 \"{recv}[{st}..]\" (or append #[allow] to this line to keep it)"
            ),
            end.span.clone(),
        ));
    }

    /// `x[0..y]` — a start bound of literal `0` is what the open-ended form
    /// already means; recommend `x[..y]`. The mirror of [`slice_to_len`], and it
    /// leans on the same distinction: an *omitted* start is synthesized as a
    /// zero with an **empty** span, so requiring a non-empty span is what
    /// separates a written `0` from an elided one.
    ///
    /// Only flagged when there is an end bound. `x[0..]` is left alone on
    /// purpose: `x[..]` is not a form, so there would be nothing to recommend.
    fn slice_from_zero(e: &Expr, src: &str, hits: &mut Vec<Error>) {
        let ExprKind::Slice(obj, start, Some(end)) = &e.kind else {
            return;
        };
        if !matches!(start.kind, ExprKind::Num(0)) || start.span.is_empty() {
            return;
        }
        // `x[0..x.len()]` is the whole receiver — [`slice_whole`]'s to report,
        // and in one step rather than the two this lint's `x[..x.len()]` would
        // start.
        if end_is_receiver_len(end, obj) {
            return;
        }
        // The end bound is deliberately *not* quoted back: a call-shaped end
        // (`s.len()`) has a span that stops before its parens, so splicing the
        // source text produced malformed advice like `s[..s.len]`.
        let recv = &src[obj.span.clone()];
        hits.push(Error::at(
            format!(
                "slice starts at 0 — drop it for the open-ended \"{recv}[..end]\" \
                 (or append #[allow] to this line to keep it)"
            ),
            start.span.clone(),
        ));
    }

    /// `|x| f(x)` / `|x| x.f()` — a lambda whose body only forwards its
    /// parameters, unchanged and in order, to a single call. A named function
    /// (or a function-typed value) can be passed directly, so recommend passing
    /// `f` itself. Purely syntactic: the call's arguments must be exactly the
    /// lambda parameters as bare identifiers, in order and in full — a captured
    /// or reordered argument, an extra argument, or an unused parameter all
    /// leave it un-flagged. The callee name must not itself be one of the
    /// parameters (that's self-application, `|x| x(x)`, not forwarding). Method
    /// form (`x.f()`) is stored as the free call `f(x)`, so it's covered too.
    /// `match (o) { some(v) => p, none => false }` — an optional matched only to
    /// answer a yes/no question, with `none` spelled as `false`. That is exactly
    /// `o.is_some_and(|v| p)`, which says the same thing in one line and without
    /// naming the `none` case at all.
    ///
    /// The `none` arm being the literal `false` is what makes this recognizable
    /// without types: arms must agree on a type, so a `false` arm proves the
    /// whole `match` is a `bool`, and `some`/`none` patterns only ever match an
    /// optional. Arm order doesn't matter — `none` may be written first.
    ///
    /// Two near misses are deliberately left alone, because neither has an
    /// `is_some_and` spelling that improves on what is written: an arm that
    /// computes its `bool` some other way (`none => flag`) still has to run that
    /// expression, and `some(v) => true` ignores the payload entirely — that one
    /// is an `is_some`, which is not a builtin.
    fn match_is_some_and(e: &Expr, hits: &mut Vec<Error>) {
        let ExprKind::Match(scrut, arms) = &e.kind else {
            return;
        };
        if arms.len() != 2 {
            return;
        }
        let arm_named = |want: &str| {
            arms.iter()
                .find(|a| matches!(&a.pattern, Pattern::Ctor { name, .. } if name == want))
        };
        let (Some(some_arm), Some(none_arm)) = (arm_named("some"), arm_named("none")) else {
            return;
        };
        let Pattern::Ctor { bindings, .. } = &some_arm.pattern else {
            return;
        };
        let Pattern::Ctor {
            bindings: none_binds,
            ..
        } = &none_arm.pattern
        else {
            return;
        };
        // `some` binds exactly its payload; `none` binds nothing.
        let [binder] = &bindings[..] else {
            return;
        };
        if !none_binds.is_empty() || !matches!(none_arm.body.kind, ExprKind::Bool(false)) {
            return;
        }
        // `some(v) => true` is not an `is_some_and` — it asks only whether the
        // optional is populated, ignoring the payload. The honest rewrite is an
        // `is_some`, which doesn't exist as a builtin; advising
        // `is_some_and(|v| true)` here would be worse than what's written. Left
        // alone until there is something better to point at.
        if matches!(some_arm.body.kind, ExprKind::Bool(true)) {
            return;
        }
        // Quote the scrutinee back only when it is a bare name, whose span is
        // exactly its text. A call-shaped scrutinee's span stops before its
        // parens (see `slice_from_zero`), so splicing it would produce advice
        // that doesn't parse.
        let call = match &scrut.kind {
            ExprKind::Ident(name) => format!("{name}.is_some_and(|{binder}| ..)"),
            _ => format!(".is_some_and(|{binder}| ..)"),
        };
        // Point at the `none => false` arm, not the whole `match`. `#[allow]` is
        // line-scoped (see `allow_squelch`), so a hit spanning several lines
        // could never be squelched: the only line able to carry the marker is
        // the `match (..) {` header, and the formatter moves a marker written
        // there onto its own line, where it squelches nothing. The `false` arm
        // is a literal, so it is always one line and always takes a trailing
        // marker — and it is the part that makes this shape recognizable.
        hits.push(Error::at(
            format!(
                "this `match` only asks whether the optional holds a value the arm \
                 accepts — write \"{call}\" instead (or append #[allow] to this line \
                 to keep it)"
            ),
            none_arm.body.span.clone(),
        ));
    }

    /// Whether a `none` arm's value is built purely from constants, and so costs
    /// the same computed eagerly as computed lazily.
    ///
    /// Literals and names, and anything assembled out of them: a struct or
    /// variant construction (a range literal `0..0` is one of these — it parses
    /// to a `__builtin_Span` construction), an array/set/dict/tuple literal, a
    /// field read, a negation. Arithmetic counts too, except `/` and `%`, which
    /// trap on a zero divisor — evaluating one eagerly could turn a program that
    /// returns into one that dies.
    ///
    /// A *call* never counts, however cheap it looks: what it costs, and whether
    /// it has effects of its own, is not visible from here.
    fn constant_default(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Num(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Ident(_)
            | ExprKind::None
            | ExprKind::Unit => true,
            ExprKind::Construct(_, inits) => inits.iter().all(|i| constant_default(&i.value)),
            ExprKind::ArrayLit(xs) | ExprKind::SetLit(xs) | ExprKind::TupleLit(xs) => {
                xs.iter().all(constant_default)
            }
            ExprKind::DictLit(pairs) => pairs
                .iter()
                .all(|(k, v)| constant_default(k) && constant_default(v)),
            ExprKind::Field(x, _) | ExprKind::Neg(x) | ExprKind::Not(x) => constant_default(x),
            ExprKind::Binop(a, op, b) => {
                *op != '/' && *op != '%' && constant_default(a) && constant_default(b)
            }
            _ => false,
        }
    }

    /// `match (o) { some(v) => v, none => d }` — an optional unwrapped to its
    /// payload with a fallback. That is exactly `o.value_or(d)`.
    ///
    /// The `some` arm must hand back its own binding unchanged; that identity
    /// arm is what separates this from a `map`, where the payload is transformed
    /// on the way out. Arm order doesn't matter.
    ///
    /// The default must be built from constants — see [`constant_default`].
    /// `value_or`'s default is an ordinary call argument, so it is evaluated
    /// whether or not the optional is empty, while a `none` arm runs only when
    /// it is; advising the rewrite for a default that *does* work would move
    /// that work to where it did not happen before.
    ///
    /// The bound used to be a literal or a bare name, which excluded the shape
    /// this rule most wants to catch: a `none` arm holding an inline struct or
    /// variant. That was not a formatting limit — `value_or` genuinely could not
    /// unwrap such an optional, and said so — but it is AIPL-implemented now and
    /// generic over every payload.
    fn match_value_or(e: &Expr, src: &str, hits: &mut Vec<Error>) {
        let ExprKind::Match(scrut, arms) = &e.kind else {
            return;
        };
        if arms.len() != 2 {
            return;
        }
        let arm_named = |want: &str| {
            arms.iter()
                .find(|a| matches!(&a.pattern, Pattern::Ctor { name, .. } if name == want))
        };
        let (Some(some_arm), Some(none_arm)) = (arm_named("some"), arm_named("none")) else {
            return;
        };
        let Pattern::Ctor { bindings, .. } = &some_arm.pattern else {
            return;
        };
        let Pattern::Ctor {
            bindings: none_binds,
            ..
        } = &none_arm.pattern
        else {
            return;
        };
        // `some` binds exactly its payload; `none` binds nothing.
        let [binder] = &bindings[..] else {
            return;
        };
        if !none_binds.is_empty() {
            return;
        }
        // The identity arm. `some(v) => f(v)` is a `map`, not a `value_or`.
        let ExprKind::Ident(payload) = &some_arm.body.kind else {
            return;
        };
        if payload != binder {
            return;
        }
        if !constant_default(&none_arm.body) {
            return;
        }
        // The default's own span stops at the last *token* the expression
        // started with — for a struct literal, before its closing brace — so
        // splicing it alone yields advice that doesn't parse. The arm's span
        // reaches the end of the arm, so slice from the body's start to there
        // and drop the separator the arm may end with.
        // A single-token default splices back from source verbatim — its span is
        // exactly the token, escapes and quotes included, which reconstructing
        // from the AST would have to re-derive. Anything wider does not: a
        // construction's span stops at its last *token*, before the closing
        // brace, and `MatchArm::span` covers only the pattern, so no range
        // reliably spells the whole default. Rather than emit advice that
        // doesn't parse, the message then names the default instead of quoting
        // it.
        let default: Option<&str> = matches!(
            none_arm.body.kind,
            ExprKind::Num(_)
                | ExprKind::Bool(_)
                | ExprKind::Str(_)
                | ExprKind::Char(_)
                | ExprKind::Ident(_)
        )
        .then(|| src.get(none_arm.body.span.start..none_arm.body.span.end))
        .flatten();
        // Quote the scrutinee back only when it is a bare name, whose span is
        // exactly its text — a call-shaped scrutinee's span stops before its
        // parens (see `slice_from_zero`), so splicing it would produce advice
        // that doesn't parse.
        // Quote the scrutinee back only when it is a bare name, whose span is
        // exactly its text — a call-shaped scrutinee's span stops before its
        // parens (see `slice_from_zero`), so splicing it would produce advice
        // that doesn't parse.
        let receiver = match &scrut.kind {
            ExprKind::Ident(name) => name.as_str(),
            _ => "",
        };
        let advice = match default {
            Some(d) => format!("write \"{receiver}.value_or({d})\" instead"),
            None => format!(
                "write it as \"{receiver}.value_or(..)\" with that arm's value as the default"
            ),
        };
        // Point at the `some(v) => v` arm's body, not the whole `match`.
        // `#[allow]` is line-scoped (see `allow_squelch`), so a hit spanning the
        // multi-line `match` could never be squelched. That lone identifier is
        // always one line, so it always takes a trailing marker and keeps it
        // through `aipl fmt` — and it is the arm that identifies the shape, since
        // a default alone is something `value_or` has too.
        hits.push(Error::at(
            format!(
                "this `match` hands back the optional's payload or a default — \
                 {advice} (or append #[allow] to this line to keep it)"
            ),
            some_arm.body.span.clone(),
        ));
    }

    /// `match (r) { ok(v) => ok(v), err(e) => err(g) }` — a result whose error
    /// side is rebuilt and whose value passes straight through. That is exactly
    /// `r.map_err(|e| g)`.
    ///
    /// The `ok` arm handing its own binding back to `ok` is what makes this a
    /// `map_err` and not a `match` that transforms both sides. Arm order doesn't
    /// matter.
    ///
    /// Unlike [`match_value_or`], there is no eager/lazy question to guard
    /// against and so no restriction on what the `err` arm builds: `map_err`
    /// takes a *function*, which runs on the error path exactly when the arm
    /// did. The one shape left alone is `err(e) => err(e)` — a `match` that
    /// rebuilds its scrutinee unchanged is a no-op, and `map_err(|e| e)` would
    /// be a longer way to write the same nothing.
    fn match_map_err(e: &Expr, hits: &mut Vec<Error>) {
        let ExprKind::Match(scrut, arms) = &e.kind else {
            return;
        };
        if arms.len() != 2 {
            return;
        }
        let arm_named = |want: &str| {
            arms.iter()
                .find(|a| matches!(&a.pattern, Pattern::Ctor { name, .. } if name == want))
        };
        let (Some(ok_arm), Some(err_arm)) = (arm_named("ok"), arm_named("err")) else {
            return;
        };
        let Pattern::Ctor {
            bindings: ok_binds, ..
        } = &ok_arm.pattern
        else {
            return;
        };
        let Pattern::Ctor {
            bindings: err_binds,
            ..
        } = &err_arm.pattern
        else {
            return;
        };
        // Each side binds exactly its payload. A unit-ok result (`ok => ok`)
        // binds nothing and has no payload to pass through, so it isn't this.
        let ([ok_binder], [err_binder]) = (&ok_binds[..], &err_binds[..]) else {
            return;
        };
        // The identity arm: `ok(v) => ok(v)`, the same binding straight back.
        let ExprKind::Call(ok_name, ok_args, _) = &ok_arm.body.kind else {
            return;
        };
        let [ok_arg] = &ok_args[..] else {
            return;
        };
        if ok_name != "ok" || !matches!(&ok_arg.kind, ExprKind::Ident(v) if v == ok_binder) {
            return;
        }
        // The error arm must rebuild an `err`; anything else is a `match` that
        // leaves the result world, which `map_err` cannot express.
        let ExprKind::Call(err_name, err_args, _) = &err_arm.body.kind else {
            return;
        };
        let [err_arg] = &err_args[..] else {
            return;
        };
        if err_name != "err" {
            return;
        }
        // `err(e) => err(e)` — see the doc comment: a no-op, left alone.
        if matches!(&err_arg.kind, ExprKind::Ident(v) if v == err_binder) {
            return;
        }
        // Quote the scrutinee back only when it is a bare name, whose span is
        // exactly its text — a call-shaped scrutinee's span stops before its
        // parens (see `slice_from_zero`), so splicing it would produce advice
        // that doesn't parse.
        let call = match &scrut.kind {
            ExprKind::Ident(name) => format!("{name}.map_err(|{err_binder}| ..)"),
            _ => format!(".map_err(|{err_binder}| ..)"),
        };
        // Point at the `ok(v) => ok(v)` arm's body, not the whole `match`.
        // `#[allow]` is line-scoped (see `allow_squelch`), so a hit spanning the
        // multi-line `match` could never be squelched. That pass-through is
        // always one line — the `err` arm's replacement need not be — and it is
        // the arm that identifies the shape.
        hits.push(Error::at(
            format!(
                "this `match` rewrites the result's error side and passes its value \
                 through — write \"{call}\" instead (or append #[allow] to this line \
                 to keep it)"
            ),
            ok_arm.body.span.clone(),
        ));
    }

    /// `match (r) { ok(v) => ok(g), err(e) => err(e) }` — a result whose value
    /// side is rebuilt and whose error passes straight through. That is exactly
    /// `r.map_ok(|v| g)`, and the mirror of [`match_map_err`].
    ///
    /// The `err` arm handing its own binding back to `err` is what makes this a
    /// `map_ok`; the `ok` arm must do something, since `ok(v) => ok(v)` alongside
    /// it is a `match` that rebuilds its scrutinee unchanged. Neither lint fires
    /// on that no-op: advising `map_ok(|v| v)` would be a longer way to write
    /// the same nothing. Arm order doesn't matter.
    ///
    /// As with `map_err` there is no eager/lazy question — `map_ok` takes a
    /// function, which runs on the value path exactly when the arm did — and so
    /// no restriction on what the `ok` arm builds.
    fn match_map_ok(e: &Expr, hits: &mut Vec<Error>) {
        let ExprKind::Match(scrut, arms) = &e.kind else {
            return;
        };
        if arms.len() != 2 {
            return;
        }
        let arm_named = |want: &str| {
            arms.iter()
                .find(|a| matches!(&a.pattern, Pattern::Ctor { name, .. } if name == want))
        };
        let (Some(ok_arm), Some(err_arm)) = (arm_named("ok"), arm_named("err")) else {
            return;
        };
        let Pattern::Ctor {
            bindings: ok_binds, ..
        } = &ok_arm.pattern
        else {
            return;
        };
        let Pattern::Ctor {
            bindings: err_binds,
            ..
        } = &err_arm.pattern
        else {
            return;
        };
        // Each side binds exactly its payload. A unit-ok result (`ok => ok`) has
        // no value to rewrite, so it isn't this.
        let ([ok_binder], [err_binder]) = (&ok_binds[..], &err_binds[..]) else {
            return;
        };
        // The identity arm: `err(e) => err(e)`, the same binding straight back.
        let ExprKind::Call(err_name, err_args, _) = &err_arm.body.kind else {
            return;
        };
        let [err_arg] = &err_args[..] else {
            return;
        };
        if err_name != "err" || !matches!(&err_arg.kind, ExprKind::Ident(v) if v == err_binder) {
            return;
        }
        // The value arm must rebuild an `ok`; anything else is a `match` that
        // leaves the result world, which `map_ok` cannot express.
        let ExprKind::Call(ok_name, ok_args, _) = &ok_arm.body.kind else {
            return;
        };
        let [ok_arg] = &ok_args[..] else {
            return;
        };
        if ok_name != "ok" {
            return;
        }
        // `ok(v) => ok(v)` — see the doc comment: with both arms identity the
        // whole `match` is a no-op, left alone by this lint and by `map_err`'s.
        if matches!(&ok_arg.kind, ExprKind::Ident(v) if v == ok_binder) {
            return;
        }
        // Quote the scrutinee back only when it is a bare name, whose span is
        // exactly its text — a call-shaped scrutinee's span stops before its
        // parens (see `slice_from_zero`), so splicing it would produce advice
        // that doesn't parse.
        let call = match &scrut.kind {
            ExprKind::Ident(name) => format!("{name}.map_ok(|{ok_binder}| ..)"),
            _ => format!(".map_ok(|{ok_binder}| ..)"),
        };
        // Point at the `err(e) => err(e)` arm's body, not the whole `match`.
        // `#[allow]` is line-scoped (see `allow_squelch`), so a hit spanning the
        // multi-line `match` could never be squelched. That pass-through is
        // always one line — the `ok` arm's replacement need not be — and it is
        // the arm that identifies the shape.
        hits.push(Error::at(
            format!(
                "this `match` rewrites the result's value side and passes its error \
                 through — write \"{call}\" instead (or append #[allow] to this line \
                 to keep it)"
            ),
            err_arm.body.span.clone(),
        ));
    }

    fn eta_lambda(e: &Expr, hits: &mut Vec<Error>) {
        let ExprKind::Lambda(params, body) = &e.kind else {
            return;
        };
        let ExprKind::Call(name, args, _) = &body.kind else {
            return;
        };
        if args.len() != params.len() || params.iter().any(|p| &p.name == name) {
            return;
        }
        for (arg, param) in args.iter().zip(params) {
            let ExprKind::Ident(a) = &arg.kind else {
                return;
            };
            if a != &param.name {
                return;
            }
        }
        hits.push(Error::at(
            format!(
                "lambda only forwards its argument(s) to \"{name}\" — pass \
                 \"{name}\" directly (or append #[allow] to this line to keep it)"
            ),
            e.span.clone(),
        ));
    }

    /// `Point { x: x }` — a field initialized with the bare identifier of the
    /// same name; recommend the shorthand `Point { x }`. The shorthand desugars
    /// to exactly this AST, so the two forms are indistinguishable by shape
    /// alone: the explicit form is identified from source by a `:` immediately
    /// before the value (the shorthand's value span *is* the field name, with no
    /// colon before it). A comment between the `:` and the value leaves it
    /// un-flagged — the conservative choice that never mis-flags a shorthand.
    fn field_init_shorthand(e: &Expr, src: &str, hits: &mut Vec<Error>) {
        let ExprKind::Construct(name, inits) = &e.kind else {
            return;
        };
        // Synthetic constructs the parser desugars from other syntax carry a
        // `__builtin_`-prefixed name a user can't write — e.g. a range
        // `start..end` becomes `__builtin_Span { start: start, end: end }`. The
        // bound identifiers aren't a written `field: field`, so skip these (the
        // source-scan below would otherwise read the *outer* field's colon).
        if name.starts_with("__builtin_") {
            return;
        }
        for init in inits {
            let ExprKind::Ident(n) = &init.value.kind else {
                continue;
            };
            if n != &init.name {
                continue;
            }
            if !src[..init.value.span.start].trim_end().ends_with(':') {
                continue; // already the shorthand form
            }
            hits.push(Error::at(
                format!(
                    "field \"{n}\" is set to the identifier of the same name — use the \
                     shorthand \"{n}\" (or append #[allow] to this line to keep it)"
                ),
                init.value.span.clone(),
            ));
        }
    }

    /// `set x = x + 1;` — an increment written the long way; `set x++;` is the
    /// form for it. Either operand order counts (`x + 1` and `1 + x` both add
    /// one), and `set x++;` itself never trips this: it parses as the *increment*
    /// operator (`'P'`), which is a different node entirely.
    ///
    /// Only a bare-identifier LHS is flagged, because that is all `set x++;`
    /// accepts — a field store (`set p.n = p.n + 1;`) has no shorter spelling to
    /// recommend.
    ///
    /// `incr` is the `++` import to recommend, from [`matching_increment`]: the
    /// flavor that adds the way this file's `+` does. `None` when the file
    /// already imports it, so the message stays short.
    fn incr_by_one(e: &Expr, incr: Option<&str>, hits: &mut Vec<Error>) {
        let ExprKind::Assign(lhs, value, _) = &e.kind else {
            return;
        };
        let ExprKind::Ident(name) = &lhs.kind else {
            return;
        };
        let ExprKind::Binop(l, '+', r) = &value.kind else {
            return;
        };
        let is_one = |e: &Expr| matches!(e.kind, ExprKind::Num(1));
        let is_target = |e: &Expr| matches!(&e.kind, ExprKind::Ident(n) if n == name);
        if !((is_target(l) && is_one(r)) || (is_one(l) && is_target(r))) {
            return;
        }
        // Name the import too when it's missing: `++` has no bare form, so
        // following the advice without it only trades this error for the
        // operator gate's.
        let import = incr
            .map(|n| format!(", importing `{n} as ++` from builtins"))
            .unwrap_or_default();
        hits.push(Error::at(
            format!(
                "adding 1 to \"{name}\" — use the increment form \"set {name}++;\"{import} \
                 (or append #[allow] to this line to keep it)"
            ),
            e.span.clone(),
        ));
    }

    /// The `++` flavor that increments the way this file's `+` adds, and whether
    /// it still needs importing — `Some(None)` when the file already has it,
    /// `Some(Some(name))` when [`incr_by_one`] should name it, and `None` when
    /// the lint must stay quiet.
    ///
    /// Both operators are bound by import and neither binding is fixed, so the
    /// two spellings only agree when they resolve to the same implementation:
    /// `wrapping_add as +` pairs with `wrapping_increment as ++`, and
    /// `saturating_add as +` with `saturating_increment as ++` — each pair
    /// sharing one `__builtin_*_add` (see `OPERATOR_BUILTINS`). The lint stays
    /// quiet when the file's `+` is a user function (no `++` flavor could match
    /// it), and when its `++` is already bound to the *other* flavor, where
    /// `set x++;` would add differently than the `+ 1` it replaces.
    fn matching_increment(program: &Program) -> Option<Option<&'static str>> {
        // What each operator's import resolves to: `None` for unbound, and
        // `Some(None)` for a binding that isn't an operator builtin at all (a
        // user function), which no `++` flavor can match.
        let (mut plus, mut incr) = (None, None);
        for item in &program.items {
            let Item::Import(decl) = item else {
                continue;
            };
            let from_builtins = matches!(decl.source, ImportSource::Builtins { .. });
            for n in &decl.names {
                let canonical = from_builtins
                    .then(|| super::operator_builtin(&n.name))
                    .flatten()
                    .map(|(_, canonical)| canonical);
                match n.local() {
                    "+" => plus = Some(canonical),
                    "++" => incr = Some(canonical),
                    _ => {}
                }
            }
        }
        let plus = plus??;
        let name = super::operator_builtin_named("++", plus)?;
        match incr {
            // Already imported, and it adds the same way: recommend the form alone.
            Some(Some(c)) if c == plus => Some(None),
            // Bound to something else — the other flavor, or a user function.
            Some(_) => None,
            None => Some(Some(name)),
        }
    }

    /// What [`len_gt_zero`] needs from the file's imports: whether `<` and `>`
    /// are the comparisons they look like, and the local name `is_nonempty`
    /// goes by here (`None` when the file hasn't imported it yet).
    struct LenZeroCmp {
        lt: bool,
        gt: bool,
        nonempty: Option<String>,
    }

    /// Read those three names out of the import list. Operators are bound by
    /// import and nothing fixes that binding, so a file that aliases `<` to a
    /// function of its own has a `0 < x.len()` that is not a length test at all
    /// — the lint stays quiet there, the way [`incr_by_one`] bows out when a
    /// file's `++` and `+` disagree. Only a builtins import counts: the operator
    /// builtins are the ones whose meaning is fixed.
    fn len_zero_cmp(program: &Program) -> LenZeroCmp {
        let mut cmp = LenZeroCmp {
            lt: false,
            gt: false,
            nonempty: None,
        };
        for item in &program.items {
            let Item::Import(decl) = item else {
                continue;
            };
            if !matches!(decl.source, ImportSource::Builtins { .. }) {
                continue;
            }
            for n in &decl.names {
                // A single-semantics operator's canonical impl is the operator
                // spelling itself (see `OPERATOR_BUILTINS`), so the operator this
                // name provides is all there is to check.
                let op = super::operator_builtin(&n.name).map(|(op, _)| op);
                match n.local() {
                    "<" => cmp.lt = op == Some("<"),
                    ">" => cmp.gt = op == Some(">"),
                    // Aliasable like any builtin, and the advice below has to
                    // spell whatever this file calls it.
                    _ if n.name == "is_nonempty" => cmp.nonempty = Some(n.local().to_string()),
                    _ => {}
                }
            }
        }
        cmp
    }

    /// `0 < x.len()` / `x.len() > 0` — a length computed only to be compared
    /// against zero. `x.is_nonempty()` asks the question directly: it says what
    /// the test means rather than leaving the reader to recognize the idiom, and
    /// it is the one spelling that reads the same for every receiver `len`
    /// takes.
    ///
    /// Only these two orders qualify. `0 > x.len()` and `x.len() < 0` are
    /// something else entirely (both are always false on an unsigned length),
    /// and the `!= 0` spellings are left alone: this lint's job is the pair of
    /// idioms people actually reach for, not every expression that happens to
    /// mention a length and a zero.
    fn len_gt_zero(e: &Expr, src: &str, cmp: &LenZeroCmp, hits: &mut Vec<Error>) {
        let ExprKind::Binop(l, op, r) = &e.kind else {
            return;
        };
        let is_zero = |e: &Expr| matches!(e.kind, ExprKind::Num(0));
        let recv = match op {
            '<' if cmp.lt && is_zero(l) => len_receiver(r),
            '>' if cmp.gt && is_zero(r) => len_receiver(l),
            _ => None,
        };
        let Some(recv) = recv else {
            return;
        };
        let name = cmp.nonempty.as_deref().unwrap_or("is_nonempty");
        // Name the import too when it's missing — like `++`, the builtin has to
        // be imported before the advice can be followed.
        let import = match cmp.nonempty {
            Some(_) => String::new(),
            None => format!(", importing `{name}` from builtins"),
        };
        // Quote the receiver only where the rewrite is a literal splice — the
        // method spelling, whose `.len` follows the receiver's span verbatim.
        // That check is what keeps a parenthesized or free-call receiver
        // (`len(f(x))`, `(a + b).len()`) from being spliced into advice that
        // would reassociate it.
        let advice = if src[recv.span.end..].starts_with(".len") {
            format!("use \"{}.{name}()\"", &src[recv.span.clone()])
        } else {
            format!("use \"{name}\" on the receiver")
        };
        hits.push(Error::at(
            format!(
                "a length compared against 0 — {advice}{import} (or append #[allow] to this line \
                 to keep it)"
            ),
            e.span.clone(),
        ));
    }

    /// The receiver of a `len` call, however it was spelled (`x.len()` is stored
    /// as the free call `len(x)`), or `None` when `e` is not one.
    fn len_receiver(e: &Expr) -> Option<&Expr> {
        let ExprKind::Call(name, args, _) = &e.kind else {
            return None;
        };
        (name == "len" && args.len() == 1).then(|| &args[0])
    }

    /// The local names this file's imports give the three builtins the
    /// [`push_loop_pipeline`] rewrite is written in terms of. `None` means the
    /// file hasn't imported that one — which for `push` means the lint stays
    /// quiet (a `push` it never imported is some other function), and for
    /// `map`/`filter` only means the advice has to name the import too, the way
    /// [`len_gt_zero`] names `is_nonempty`.
    ///
    /// Only a builtins import counts, and each is matched on its *exported*
    /// name so an alias (`filter as keep`) is followed rather than missed.
    struct PipelineNames {
        push: Option<String>,
        map: Option<String>,
        filter: Option<String>,
    }

    fn pipeline_names(program: &Program) -> PipelineNames {
        let mut names = PipelineNames {
            push: None,
            map: None,
            filter: None,
        };
        for item in &program.items {
            let Item::Import(decl) = item else {
                continue;
            };
            if !matches!(decl.source, ImportSource::Builtins { .. }) {
                continue;
            }
            for n in &decl.names {
                let local = || Some(n.local().to_string());
                match n.name.as_str() {
                    "push" => names.push = local(),
                    "map" => names.map = local(),
                    "filter" => names.filter = local(),
                    _ => {}
                }
            }
        }
        names
    }

    /// Whether `e` can be moved into a lambda unchanged.
    ///
    /// Two things stop it. Mentioning `acc` — the array being accumulated —
    /// means the expression reads a value the rewritten pipeline no longer has
    /// (`out.len()` as a running index is the shape this catches). And a `?`
    /// propagation returns from the function it is written in, so relocating it
    /// into a lambda would return from the *lambda* instead: same source, a
    /// different early exit.
    fn lambda_safe(e: &Expr, acc: &str) -> bool {
        let mut ok = true;
        super::each_subexpr(e, &mut |x| match &x.kind {
            ExprKind::Ident(n) if n == acc => ok = false,
            ExprKind::Try(_) => ok = false,
            _ => {}
        });
        ok
    }

    /// The synthetic value the parser ends a statement-only block with: `()` for
    /// an ordinary block, and an i64 `0` — the loop expression's own result —
    /// for a loop body, which may not end in a value of its own at all. The
    /// loop's zero is told from a written one by its empty span, the same way
    /// [`slice_from_zero`] tells an elided slice bound from a typed one.
    fn is_block_tail(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Unit => true,
            ExprKind::Num(0) => e.span.is_empty(),
            _ => false,
        }
    }

    /// The single statement a block consists of, or `None` when it holds
    /// anything else — no statements, or more than one.
    ///
    /// A block is a right-nested chain, and which node carries the rest of it
    /// depends on the statement: an expression statement is a [`ExprKind::Seq`]
    /// whose second operand is the remainder, while `let`/`mut`/`set` each hold
    /// their own. Only the two forms a push loop can be made of are recognized
    /// here, and each is "lone" exactly when what follows it is the block's
    /// tail (see [`is_block_tail`]).
    fn lone_stmt(block: &Expr) -> Option<&Expr> {
        match &block.kind {
            ExprKind::Seq(stmt, rest) if is_block_tail(rest) => Some(stmt),
            ExprKind::Assign(_, _, rest) if is_block_tail(rest) => Some(block),
            _ => None,
        }
    }

    /// `set acc.push(elem);` (or its longhand `set acc = acc.push(elem);`) —
    /// the pushed element, when `stmt` is a push onto `acc` and nothing else.
    /// `push` is this file's local name for the builtin, so an aliased import is
    /// followed and a *user* function that happens to be called `push` is not.
    fn pushed_element<'a>(stmt: &'a Expr, acc: &str, push: &str) -> Option<&'a Expr> {
        let ExprKind::Assign(lhs, value, _) = &stmt.kind else {
            return None;
        };
        if !matches!(&lhs.kind, ExprKind::Ident(n) if n == acc) {
            return None;
        }
        let ExprKind::Call(name, args, _) = &value.kind else {
            return None;
        };
        if name != push || args.len() != 2 {
            return None;
        }
        if !matches!(&args[0].kind, ExprKind::Ident(n) if n == acc) {
            return None;
        }
        Some(&args[1])
    }

    /// Whether `e`'s span covers exactly the text that spells it, so the source
    /// can be spliced back into advice. Only a name and a field path off one
    /// qualify: their spans run from the first character to the last. Most other
    /// forms do not — a call's span stops before its closing paren, so `f(x)`
    /// splices back as `f(x`, which doesn't parse — and the ones that might are
    /// left out rather than each having to be re-checked whenever a span moves.
    fn spans_its_text(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Ident(_) => true,
            ExprKind::Field(recv, _) => spans_its_text(recv),
            _ => false,
        }
    }

    /// ```text
    /// mut out = [];
    /// for (let x : xs) {
    ///     if (keep(x)) {
    ///         set out.push(f(x));
    ///     };
    /// }
    /// ```
    ///
    /// — an array seeded empty and filled by a loop that does nothing but test
    /// each element and push something derived from it. That is
    /// `xs.filter(|x| keep(x)).map(|x| f(x))`: one expression that says the
    /// array *is* the selected elements, rather than three statements the reader
    /// has to run in their head to find out that nothing else happens to `out`.
    ///
    /// Both halves are optional, and which are present picks the advice: with no
    /// guard it is a `map`, with the element pushed unchanged a `filter`, and
    /// with neither the loop is copying `xs` element by element and `xs` itself
    /// is the answer.
    ///
    /// The shape has to be *exactly* this, because anything else in the loop is
    /// something the pipeline would drop: the loop body is one statement (the
    /// push, or an else-less `if` around one push), the guard and the pushed
    /// element may not mention `out` (see [`lambda_safe`]), and the file's
    /// `push` must be the builtin. What comes *after* the loop is unconstrained
    /// — a later `set out.push(..)` is fine, since the rewrite leaves `out`
    /// a `mut` binding that simply starts out filled.
    ///
    /// The one thing this cannot check is that `xs` is an array: `map` and
    /// `filter` take arrays, but a `for` also iterates a `str`'s bytes, and
    /// which one this is only becomes known two passes later. A `str` loop that
    /// collects into an array is thus the lint's blind spot, and `#[allow]` is
    /// the answer there.
    fn push_loop_pipeline(e: &Expr, src: &str, names: &PipelineNames, hits: &mut Vec<Error>) {
        let Some(push) = &names.push else {
            return;
        };
        // `mut acc = [];` — the seed. An annotation is allowed (and usual: it is
        // often what gives the empty literal its element type) but says nothing
        // about the iterable, so it is not inspected.
        let ExprKind::LetMut(acc, _, seed, body) = &e.kind else {
            return;
        };
        if !matches!(&seed.kind, ExprKind::ArrayLit(xs) if xs.is_empty()) {
            return;
        }
        // The very next statement must be the loop: a statement between the two
        // could read `acc` while it is still empty, which the rewrite would
        // reorder. (A `for` is folded as `Seq(For, rest)` — see `wrap_stmt`.)
        let ExprKind::Seq(first, _) = &body.kind else {
            return;
        };
        let ExprKind::For(var, iterable, loop_body) = &first.kind else {
            return;
        };
        let Some(stmt) = lone_stmt(loop_body) else {
            return;
        };
        // An else-less `if` around the push is the filter; anything else in the
        // body (an `else`, a second statement) is work the pipeline has nowhere
        // to put.
        let (guard, stmt) = match &stmt.kind {
            ExprKind::If(cond, then, els) if matches!(els.kind, ExprKind::Unit) => {
                let Some(stmt) = lone_stmt(then) else {
                    return;
                };
                (Some(&**cond), stmt)
            }
            _ => (None, stmt),
        };
        let Some(elem) = pushed_element(stmt, acc, push) else {
            return;
        };
        if guard.is_some_and(|c| !lambda_safe(c, acc)) || !lambda_safe(elem, acc) {
            return;
        }
        // A push of the loop variable itself is the identity map, which the
        // pipeline just leaves out.
        let maps = !matches!(&elem.kind, ExprKind::Ident(n) if n == var);
        // Quote the iterable back only where its span really covers its text —
        // see [`spans_its_text`]. The alternative, checking the source for the
        // header's own `)`, is exactly the case that fails: a call-shaped span
        // stops *before* its closing paren, so `evens(n)` passes that test and
        // splices back as `evens(n`.
        let recv = spans_its_text(iterable).then(|| &src[iterable.span.clone()]);
        // Name whichever of the two the file hasn't imported: like `++` and
        // `is_nonempty`, a builtin has to be in scope before the advice can be
        // followed, and following it without the import only trades this error
        // for the import gate's.
        let mut missing: Vec<&str> = Vec::new();
        let mut stage = |local: &Option<String>, builtin: &'static str| match local {
            Some(n) => n.clone(),
            None => {
                missing.push(builtin);
                builtin.to_string()
            }
        };
        let mut chain = String::new();
        if guard.is_some() {
            let name = stage(&names.filter, "filter");
            chain.push_str(&format!(".{name}(|{var}| ..)"));
        }
        if maps {
            let name = stage(&names.map, "map");
            chain.push_str(&format!(".{name}(|{var}| ..)"));
        }
        let import = if missing.is_empty() {
            String::new()
        } else {
            format!(", importing `{}` from builtins", missing.join("` and `"))
        };
        // A quotable iterable is spliced in; anything else leaves a placeholder,
        // the way `slice_from_zero` writes `[..end]`.
        let recv = recv.unwrap_or("<iterable>");
        let advice = if chain.is_empty() {
            // Neither half: the loop copies the iterable one element at a time.
            format!("it is \"{recv}\" already — assign that and drop the loop")
        } else {
            format!("write \"mut {acc} = {recv}{chain};\" and drop the loop")
        };
        // Point at the seed, not the loop. `#[allow]` is line-scoped (see
        // `allow_squelch`), so a hit spanning the loop could never be squelched:
        // the marker would have to go on the `for (..) {` header, and `aipl fmt`
        // relocates one written there onto a line of its own, where it squelches
        // nothing. `mut {acc} = [];` is a short statement that always occupies
        // one line, and it is where the shape starts.
        hits.push(Error::at(
            format!(
                "\"{acc}\" is seeded empty and filled by this loop and nothing else — \
                 {advice}{import} (or append #[allow] to this line to keep it)"
            ),
            seed.span.clone(),
        ));
    }
}
