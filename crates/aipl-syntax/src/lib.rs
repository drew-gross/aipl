//! AIPL core types shared across every compiler crate: source spans, the
//! [`Error`] type, [`DebugOptions`] tracing, the [`ast`] tree, and the
//! AST-level `Type`/builtin helpers the parser, monomorphizer, codegen, and
//! loader all need. This crate has no external dependencies, so it forms the
//! fast-to-compile base of the workspace.

/// Byte-offset range in the source string.
pub type Span = std::ops::Range<usize>;

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
            Type::Case(v) => ty_mentions_any(v),
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
    }

    impl Expr {
        pub fn new(kind: ExprKind, span: Span) -> Self {
            Self {
                kind,
                span,
                ty: None,
                value_span: None,
            }
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

    /// A binary operator, as stored in [`ExprKind::Binop`].
    ///
    /// Variants name the **builtin** an operator resolves to, never the spelling
    /// a source file imported it as — `Concat`, not `+++`. Which alias a file
    /// writes is a property of its import list (`OPERATOR_BUILTINS`); after
    /// import resolution nothing downstream should be able to tell, and with an
    /// enum nothing can, because there is no character left to compare against.
    ///
    /// Where an operator has more than one semantics (`wrapping_add` vs
    /// `saturating_add`) the *alias* records the choice, so one variant covers
    /// both — the flavor is resolved by name, not by opcode.
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
        /// `remainder`.
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
        /// `wrapping_increment` / `saturating_increment`, from `set n++`. The
        /// loader lowers it to [`BinOp::Add`] once operator gating has run; the
        /// separate variant is what lets the gate demand the `++` import rather
        /// than accepting a `+` one.
        Incr,
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
        Call(String, Vec<Expr>, bool),
        Binop(Box<Expr>, BinOp, Box<Expr>),
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

use ast::{BinOp, Primitive, Type};

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
            K::Call(name, args, _) if args.len() == 1 && matches!(&**name, "ok" | "err") => {
                let side = if name == "ok" { ok } else { err };
                Ok(flex_fit(&args[0], &Type::Unit, side)?.map(|_| target.clone()))
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
    // The operators with a single semantics. They are listed here so that *every*
    // operator is imported the same way when it is imported *as an operator*:
    // `name as op`. Reading an import list then tells you which operators a file
    // uses and, where it matters, which flavor, without the reader having to
    // know which operators happen to be ambiguous.
    //
    // Their canonical impl is the operator spelling itself: unlike `+`, there is
    // nothing to dispatch between, so the view maps straight through. That makes
    // the canonical a *marker* rather than a callable `__builtin_*`, which is why
    // a bare import of one of these names lowers its calls back to the primitive
    // node (`binop_from_spelling`, used by the loader's `rewrite_expr`) instead
    // of emitting a call to a function that does not exist.
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

/// The [`BinOp`] an operator spelling denotes, for the operator builtins whose
/// "canonical impl" is the spelling itself — a marker rather than a callable
/// function (see [`OPERATOR_BUILTINS`]). `None` for `!`, which is unary
/// (`ExprKind::Not`), and for a spelling that names a real `__builtin_*` impl.
pub fn binop_from_spelling(spelling: &str) -> Option<BinOp> {
    Some(match spelling {
        "==" => BinOp::Eq,
        "!=" => BinOp::Ne,
        "<" => BinOp::Lt,
        ">" => BinOp::Gt,
        "<=" => BinOp::Le,
        ">=" => BinOp::Ge,
        "&&" => BinOp::And,
        "||" => BinOp::Or,
        "+++" => BinOp::Concat,
        "/" => BinOp::Div,
        "%" => BinOp::Rem,
        _ => return None,
    })
}

/// How many operands the operator `spelling` takes, or `None` if it is not one
/// of the marker spellings [`binop_from_spelling`] covers plus unary `!`. Used
/// to check the arity of a bare operator-builtin *call* (`concat(a, b)`).
pub fn operator_arity(spelling: &str) -> Option<usize> {
    match spelling {
        "!" => Some(1),
        _ => binop_from_spelling(spelling).map(|_| 2),
    }
}

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
        // Lowered to `+` by the loader after operator gating; this spelling is
        // what the gate requires.
        BinOp::Incr => "++",
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
fn __builtin_is_whitespace(self: char) -> bool { false }
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
// The name of the case `self` was built with, without its payload:
// `Name("x").case_name()` is `"Name"`, and a payload-free `Space` is
// `"Space"`. The tag names the case, so this reads no payload — which is
// what lets it be generic over every variant, like `same_case`.
fn __builtin_case_name<T: variant>(self: T) -> str { "" }
// `v.case_of()` — which case `v` is, as a `Case<T>` carrying no payload. The
// counterpart to naming a case directly (`Term(Str)`): this is how a *value*
// is compared against one, since `==` on two `Case<T>`s is a tag compare.
// `same_case(a, b)` remains the way to compare two values.
fn __builtin_case_of<T: variant>(self: T) -> Case<T> { __builtin_case_of(self) }
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
// NOTE: `all`, `count_while`, `count_if`, `find_if`, `is_all_whitespace`,
// `is_some_and`, `int_parse`, `trim_while`, `try_map`, `value_or`, and
// `value_or_err` are
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
// Passes a `str` through unchanged, widens a `char` to the one-char `str`
// holding it, and converts any other type via `to_str`.
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
            ConcreteType::Case(v) => format!("Case<{v}>"),
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
        Type::Case(v) => format!("Case<{}>", type_name(v)),
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
    "case_name",
    "case_of",
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
    "is_whitespace",
    "is_digit",
    "to_digit",
    "trim_while",
    "count",
    "count_while",
    "count_if",
    "find_if",
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
        T::Case(v) => promote_ty(v, vars),
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

/// Prefix of the binding the loader synthesizes for a `..base` struct spread
/// (`aipl-loader`'s `desugar_struct_spread`). It is the one thing that ties the
/// two ends of that desugaring together: the loader emits the binding carrying
/// the target struct's type, and the checker recognizes it so a failed
/// annotation is reported as the spread the user actually wrote rather than as
/// an internal `let`. Shared rather than spelled twice, because a silent
/// mismatch would turn a good diagnostic back into a confusing one.
pub const SPREAD_BASE_PREFIX: &str = "__spread_base$";

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

/// Parameter-name prefix of the lambda `aipl_mono::lower_ctor_refs` wraps a
/// payload-carrying constructor reference in (`Circle` → `|__ctor0| Circle(__ctor0)`).
/// Shared so [`ctor_ref_case`] can recognize that lambda and read the case back
/// out of it — the two ends of one desugaring, like [`SPREAD_BASE_PREFIX`].
pub const CTOR_PARAM_PREFIX: &str = "__ctor";

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
        ast::ExprKind::Ident(n) => n.as_str(),
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
            name.as_str()
        }
        _ => return None,
    };
    qualified.split_once('@')
}

pub mod lint;
