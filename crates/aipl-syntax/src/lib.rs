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

/// Error returned by parsing or codegen. Use [`Error::render`] for the
/// human-friendly rendering with source-line context.
#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub span: Option<Span>,
    /// Secondary labeled locations, rendered as `note:` blocks after the
    /// primary caret — e.g. pointing at the *other* side of a conflict.
    pub notes: Vec<(String, Span)>,
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
        }
    }

    pub fn at(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span: Some(span),
            notes: Vec::new(),
        }
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
    }

    impl Bound {
        /// Parse a bound keyword from source text (the `any`/`ord` in
        /// `<T: any>`/`<T: ord>`), or `None` if unrecognized.
        pub fn from_name(name: &str) -> Option<Bound> {
            match name {
                "any" => Some(Bound::Any),
                "ord" => Some(Bound::Ord),
                _ => None,
            }
        }

        /// Source spelling of this bound, for error messages.
        pub fn name(&self) -> &'static str {
            match self {
                Bound::Any => "any",
                Bound::Ord => "ord",
            }
        }

        /// Does `ty` satisfy this bound? `Any` accepts everything; `Ord`
        /// accepts only the primitives usable with `<`/`>` — integers and
        /// `char`.
        pub fn accepts(&self, ty: &Type) -> bool {
            match self {
                Bound::Any => true,
                Bound::Ord => {
                    matches!(ty, Type::Primitive(p) if p.is_int() || *p == Primitive::Char)
                }
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

        /// `true` for a mutating method (`fn f(mut self: T, ...)`): it returns
        /// nothing to the user, mutates its receiver, and must be called as
        /// `v.f(...)`.
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
        pub fields: Vec<FieldDecl>,
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
        pub cases: Vec<VariantCase>,
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
        /// A name that isn't a primitive: a user struct or variant, a generic
        /// type parameter (`T`), or the builtin `Error` type. (Compiler
        /// pseudo-type sentinels that used to overload this — `__none__`,
        /// `any`, etc. — have their own dedicated variants below instead.)
        Named(String),
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
        /// The anonymous generic bound keyword `any`, as written in `any[]`/
        /// `any?` — parsed directly from source. Monomorphization's `normalize`
        /// replaces each occurrence with a synthetic named type variable before
        /// anything else sees it.
        Any,
        /// The placeholder element/inner of an untyped `none`, empty array
        /// literal (`[]`), or empty set/dict literal (`#{}`/`#{:}`) — coerces to
        /// any element/inner type at the use site (see [`is_none_inner`]).
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
        /// representation for it (see [`is_concat_str`]). Only meaningful as
        /// the type of a scalar value flowing to a `str` parameter; decays to
        /// a plain `str` once it's placed into any other container/context.
        ConcatStr,
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
    }

    impl Expr {
        pub fn new(kind: ExprKind, span: Span) -> Self {
            Self { kind, span }
        }
    }

    impl PartialEq for Expr {
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
        /// method-style, a mutating method *requires* method syntax (and a
        /// mutable variable receiver), and the free-call form of a mutating
        /// builtin (`push`) is rejected. Non-mutating calls are otherwise
        /// indifferent to it (`x.to_str()` ≡ `to_str(x)`).
        Call(String, Vec<Expr>, bool),
        Binop(Box<Expr>, char, Box<Expr>),
        Neg(Box<Expr>),
        Not(Box<Expr>),
        If(Box<Expr>, Box<Expr>, Box<Expr>),
        Construct(String, Vec<FieldInit>),
        Field(Box<Expr>, String),
        /// `let name = value; body` — immutable binding.
        Let(String, Box<Expr>, Box<Expr>),
        /// `let mut name = value; body` — mutable binding, lives in a stack slot.
        LetMut(String, Box<Expr>, Box<Expr>),
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
        ArrayLit(Vec<Expr>),
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
        /// An array-literal pattern `[e0, e1, ...] => body` (matches an array
        /// scrutinee by exact length + elementwise equality). The elements are
        /// literal expressions (validated by the checker), so they introduce no
        /// bindings, free variables, or calls.
        Array(Vec<Expr>),
        /// The wildcard / default arm `_ => body` (matches anything). Only valid
        /// for a `str` or array match, where it must be the last arm.
        Wildcard,
    }

    impl Pattern {
        /// The positional binders this pattern introduces (empty except for a
        /// constructor pattern).
        pub fn bindings(&self) -> &[String] {
            match self {
                Pattern::Ctor { bindings, .. } => bindings,
                Pattern::Str(_) | Pattern::Array(_) | Pattern::Wildcard => &[],
            }
        }

        /// The constructor name for a `Ctor` pattern; `None` otherwise.
        pub fn ctor_name(&self) -> Option<&str> {
            match self {
                Pattern::Ctor { name, .. } => Some(name),
                Pattern::Str(_) | Pattern::Array(_) | Pattern::Wildcard => None,
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

/// Retype a bare integer literal `e` (currently `ety`) to a target integer type
/// `other` — used by mono/codegen after the checker has verified the literal
/// fits, so a literal's value (already canonical when in range) flows into a
/// narrow-int context without an explicit conversion. Non-literals and
/// non-integer targets are left unchanged.
pub fn flex_int_ty(e: &ast::Expr, ety: &Type, other: &Type) -> Type {
    if let Type::Primitive(p) = other {
        if p.is_int() && ety != other && const_int(e).is_some() {
            return other.clone();
        }
    }
    ety.clone()
}

/// If `name` is a named operator builtin, the `(operator, canonical impl)` it
/// provides — the operator spelling it must be aliased to, and the reserved
/// `__builtin_*` function the operator resolves to (intrinsified by codegen). An
/// operator builtin must be imported aliased to its operator: `import {
/// wrapping_add as + } from builtins;`. Multiple builtins map to the same operator
/// (`wrapping_add`/`saturating_add` both provide `+`), letting a file pick the
/// semantics it wants; the operator use dispatches on the resolved impl, not on
/// the spelling. This registry is the single place operator builtins are declared
/// — extend it (not per-operator special-cases) to add flavors.
pub fn operator_builtin(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "wrapping_add" => Some(("+", "__builtin_wrapping_add")),
        "saturating_add" => Some(("+", "__builtin_saturating_add")),
        "wrapping_sub" => Some(("-", "__builtin_wrapping_sub")),
        "saturating_sub" => Some(("-", "__builtin_saturating_sub")),
        _ => None,
    }
}

/// Whether `s` spells a built-in operator that must be imported to be used
/// (e.g. `import { == } from builtins;`; `+` comes via `wrapping_add as +`).
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
        K::Field(x, _) | K::Try(x) | K::Return(x) | K::KwArg(_, x) => collect_operators(x, out),
        // An `Assign` LHS is a place (idents/fields only), so it can't
        // contain an operator — only the value and body need walking.
        K::Seq(a, b)
        | K::Index(a, b)
        | K::Let(_, a, b)
        | K::LetMut(_, a, b)
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
struct __builtin_Span { start: i64, end: i64 }
// A finished child process's captured output and exit status.
struct __builtin_ExecResult { stdout: str, stderr: str, exit_code: i64 }

fn __builtin_print(self: str) !prints {}
// Split on each occurrence of `sep`, returning the parts (slices/views of `self`).
fn __builtin_split(self: str, sep: str) -> str[] { [] }
// Concatenate the parts with `sep` between consecutive elements.
fn __builtin_join(self: str[], sep: str) -> str { "" }

// The file builtins return a Result; the `ok(..)` body coerces to the declared
// `..!Error` (codegen builds the real ok/err).
fn __builtin_read_file_to_string(self: str) !read_files -> str!Error { ok("") }
fn __builtin_write_string_to_file(self: str, contents: str) !write_files -> !Error { ok() }
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
// Concatenate `self` with itself `n` times; returns `""` for `n <= 0`.
fn __builtin_repeat(self: str, n: i64) -> str { "" }
// True if every byte is ASCII whitespace (or the string is empty).
fn __builtin_is_all_whitespace(self: str) -> bool { false }
// True if `self` begins / ends with the argument — `str` bytes or `T[]`
// elements (the empty pattern always matches). A str receiver is dispatched in
// the checker / codegen (the `T[]` signature doesn't unify with `str`).
fn __builtin_starts_with<T: any>(self: T[], prefix: T[]) -> bool { false }
fn __builtin_ends_with<T: any>(self: T[], suffix: T[]) -> bool { false }
// True if `self` contains the needle: a `T[]` (or `str`) needle matches as a
// contiguous subsequence (substring), a `T` (or `char`) as a single element,
// and a `T?` as its element when `some` — a `none` needle is nothing to find,
// so it's `false` (unlike `starts_with`/`ends_with`, whose `none` pattern is
// the empty pattern and matches). A str receiver is dispatched in the
// checker / codegen (the `T[]` signature doesn't unify with `str`).
fn __builtin_contains<T: any>(self: T[], needle: T[]) -> bool { false }
// Smaller / larger of two `i64`s (codegen compares and selects).
fn __builtin_min(self: i64, other: i64) -> i64 { self }
fn __builtin_max(self: i64, other: i64) -> i64 { self }
// Smallest / largest element of an array, or `none` if empty (codegen folds
// over the elements). `ord` restricts `T` to comparable elements (integer or
// char), enforced generically by the checker's bound-checking, not a special
// case for these two names.
fn __builtin_minimum<T: ord>(self: T[]) -> T? { none }
fn __builtin_maximum<T: ord>(self: T[]) -> T? { none }
fn __builtin_len<T: any>(self: T[]) -> i64 { 0 }
fn __builtin_is_some<T: any>(self: T?) -> bool { false }
// True if `self` is `some(x)` and `pred(x)` holds; `false` for `none` (the
// predicate is not called). Implemented in AIPL
// (`aipl-mono/src/builtin_is_some_and.aipl`), not codegen — this declaration
// still types calls; mono swaps in the real body.
fn __builtin_is_some_and<T: any>(self: T?, pred: (T) -> bool) -> bool { false }
// Character classification: ASCII whitespace (space/tab/newline/carriage return).
fn __builtin_is_space(self: char) -> bool { false }
// Character classification: ASCII decimal digit ('0' through '9').
fn __builtin_is_digit(self: char) -> bool { false }

// Set ops: membership and union.
fn __builtin_has<T: any>(self: #{T}, x: T) -> bool { false }
fn __builtin_union<T: any>(self: #{T}, other: #{T}) -> #{T} { self }

// Dict ops: lookup (none if absent) and membership.
fn __builtin_get<K: any, V: any>(self: #{K: V}, key: K) -> V? { none }
fn __builtin_contains_key<K: any, V: any>(self: #{K: V}, key: K) -> bool { false }

fn __builtin_value_or<T: any>(self: T?, default: T) -> T { default }
fn __builtin_map<T: any, U: any>(self: T[], f: (T) -> U) -> U[] { [] }
fn __builtin_filter<T: any>(self: T[], pred: (T) -> bool) -> T[] { self }
// True when every element satisfies `pred` (vacuously true for an empty array).
// Implemented in AIPL (`aipl-mono/src/builtin_all.aipl`), not codegen — this
// declaration still types calls; mono swaps in the real body.
fn __builtin_all<T: any>(self: T[], pred: (T) -> bool) -> bool { false }
fn __builtin_zip_with<T: any, U: any, V: any>(self: T[], other: U[], f: (T, U) -> V) -> V[] { [] }
fn __builtin_push<T: any>(mut self: T[], x: T) {}
// Reverse the elements of an array or the bytes of a string.
fn __builtin_reverse<T: any>(self: T[]) -> T[] { [] }
// Pair each element with its index: `[a, b, c].enumerate()` → `[(0,a),(1,b),(2,c)]`.
fn __builtin_enumerate<T: any>(self: T[]) -> (i64, T)[] { [] }
fn some<T: any>(x: T) -> T? { none }

// Test-runner hooks. `__assert(cond, loc)` is what `assert(cond)` lowers to
// inside a `.test({ .. })` body; the other three are called by the synthesized
// `__test_main` driver (see `build_test_program`). All are effect-free so test
// code needs no effect annotations to call them.
fn __assert(cond: bool, loc: str) {}
fn __test_begin(name: str) {}
fn __test_end() {}
fn __test_summary() -> i64 { 0 }
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
pub fn is_str_repr(t: &Type) -> bool {
    matches!(t, Type::Primitive(Primitive::Str)) || is_error(t) || is_concat_str(t)
}

pub fn type_name(t: &Type) -> String {
    match t {
        Type::Unit => "()".into(),
        Type::Primitive(p) => p.name().into(),
        Type::Named(s) => s.clone(),
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
        Type::Any => "any".into(),
        Type::NoneInner => "__none__".into(),
        Type::EmptyArrayArg => "EmptyArray".into(),
        Type::NoneLiteralArg => "NoneLiteral".into(),
        Type::ConcatStr => "__concat_str__".into(),
    }
}

/// Valid array element types: the 8-byte value types — primitives, `str`,
/// and (nested) arrays, which are themselves 8-byte heap pointers. Structs
/// and optionals are inline composites wider than 8 bytes and aren't yet
/// supported as elements.
pub fn is_array_elem(t: &Type) -> bool {
    matches!(
        t,
        Type::Primitive(Primitive::I64 | Primitive::Bool | Primitive::Char | Primitive::Str)
    ) || matches!(t, Type::Array(_))
}

/// Valid set element types: the scalar value types `i64`, `bool`, `char`, and
/// `str`. Scalars compare by value; `str` compares by content (see the set
/// runtime). Nested containers (arrays/sets/optionals/structs) are not yet
/// supported as set elements.
pub fn is_set_elem(t: &Type) -> bool {
    matches!(
        t,
        Type::Primitive(Primitive::I64 | Primitive::Bool | Primitive::Char | Primitive::Str)
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
    "to_str",
    "map",
    "filter",
    "all",
    "zip_with",
    "trim",
    "is_all_whitespace",
    "starts_with",
    "ends_with",
    "len",
    "push",
    "is_some",
    "is_some_and",
    "is_space",
    "is_digit",
    "value_or",
    "contains",
    "has",
    "read_file_to_string",
    "write_string_to_file",
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
    "enumerate",
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

/// Lints: *squelchable* errors. AIPL has no warnings — every diagnostic is an
/// error and fails the compile — but the errors this module produces (and only
/// these) can be squelched by appending `#[allow]` to the offending line. The
/// marker is line-scoped: it silences every lint whose reported span starts on
/// its line, and nothing else. Regular errors (type mismatches, unknown names,
/// parse errors, ...) take no notice of `#[allow]`.
///
/// A lint flags code that is *legal but has a clearly better spelling*; its
/// message must name that better spelling. The loader runs [`check`] on every
/// file right after parsing (the markers come from the lexer via
/// `parse_with_allows`), so lints fire before type checking.
pub mod lint {
    use super::ast::{Expr, ExprKind, Item, Program};
    use super::{Error, Span};

    /// Run every lint over `program` — function bodies, `.test` blocks, and
    /// keyword-parameter / struct-field default expressions — then drop the
    /// hits squelched by a same-line `#[allow]` (`allows` are the marker spans
    /// the lexer collected). Returns the first surviving lint error.
    pub fn check(program: &Program, src: &str, allows: &[Span]) -> Result<(), Error> {
        let allowed: std::collections::HashSet<usize> =
            allows.iter().map(|sp| line_of(src, sp.start)).collect();
        let mut hits: Vec<Error> = Vec::new();
        each_expr(program, &mut |e| slice_to_len(e, src, &mut hits));
        hits.retain(|e| match &e.span {
            Some(sp) => !allowed.contains(&line_of(src, sp.start)),
            None => true,
        });
        match hits.into_iter().next() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 0-based line number of byte offset `pos` in `src`.
    fn line_of(src: &str, pos: usize) -> usize {
        src[..pos.min(src.len())].matches('\n').count()
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
        let ExprKind::Call(name, args, _) = &end.kind else {
            return;
        };
        if name != "len" || args.len() != 1 || args[0] != **obj {
            return;
        }
        let recv = &src[obj.span.clone()];
        // `x[..x.len()]` synthesizes a zero start with an empty span; spell it
        // back out, since `x[..]` is not a form.
        let st = if start.span.is_empty() {
            "0"
        } else {
            &src[start.span.clone()]
        };
        hits.push(Error::at(
            format!(
                "slice end is the receiver's whole length — use the open-ended \
                 \"{recv}[{st}..]\" (or append #[allow] to this line to keep it)"
            ),
            end.span.clone(),
        ));
    }

    /// Visit every expression in the program, pre-order.
    fn each_expr(program: &Program, f: &mut impl FnMut(&Expr)) {
        for item in &program.items {
            match item {
                Item::Fn(func) => {
                    for p in &func.sig.params {
                        if let Some(d) = &p.default {
                            walk(d, f);
                        }
                    }
                    walk(&func.body, f);
                    if let Some(t) = &func.test_body {
                        walk(t, f);
                    }
                }
                Item::Struct(s) => {
                    for field in &s.fields {
                        if let Some(d) = &field.default {
                            walk(d, f);
                        }
                    }
                }
                Item::Variant(_) | Item::Import(_) => {}
            }
        }
    }

    fn walk(e: &Expr, f: &mut impl FnMut(&Expr)) {
        use ExprKind as K;
        f(e);
        match &e.kind {
            K::Num(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::Ident(_) | K::None | K::Unit => {}
            K::Call(_, args, _) | K::ArrayLit(args) | K::SetLit(args) | K::TupleLit(args) => {
                for a in args {
                    walk(a, f);
                }
            }
            K::Construct(_, inits) => {
                for init in inits {
                    walk(&init.value, f);
                }
            }
            K::DictLit(pairs) => {
                for (k, v) in pairs {
                    walk(k, f);
                    walk(v, f);
                }
            }
            K::Binop(a, _, b)
            | K::Seq(a, b)
            | K::Index(a, b)
            | K::Let(_, a, b)
            | K::LetMut(_, a, b)
            | K::For(_, a, b)
            | K::While(a, b) => {
                walk(a, f);
                walk(b, f);
            }
            K::Assign(a, b, c) | K::If(a, b, c) => {
                walk(a, f);
                walk(b, f);
                walk(c, f);
            }
            K::Neg(x)
            | K::Not(x)
            | K::Field(x, _)
            | K::Try(x)
            | K::Return(x)
            | K::KwArg(_, x)
            | K::Lambda(_, x) => walk(x, f),
            K::Match(scrutinee, arms) => {
                walk(scrutinee, f);
                for arm in arms {
                    walk(&arm.body, f);
                }
            }
            K::Slice(a, b, c) => {
                walk(a, f);
                walk(b, f);
                if let Some(c) = c {
                    walk(c, f);
                }
            }
        }
    }
}
