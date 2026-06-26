//! AIPL core types shared across every compiler crate: source spans, the
//! [`Error`] type, [`DebugOptions`] tracing, the [`ast`] tree, and the
//! AST-level `Type`/builtin helpers the parser, monomorphizer, codegen, and
//! loader all need. This crate has no external dependencies, so it forms the
//! fast-to-compile base of the workspace.

/// Byte-offset range in the source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const DUMMY: Span = Span { start: 0, end: 0 };

    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Smallest span covering both `self` and `other`.
    pub fn join(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
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
        match self.span {
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
    /// plain `error: ...` otherwise.
    pub fn render(&self, source: &str) -> String {
        let Some(span) = self.span else {
            return format!("error: {}", self.message);
        };
        let mut out = format!("error: {}\n{}", self.message, caret_block(source, span));
        for (note, nspan) in &self.notes {
            out.push_str(&format!("\nnote: {note}\n{}", caret_block(source, *nspan)));
        }
        out
    }
}

/// The rustc-style location + caret block for a single span (no leading label
/// line — callers prepend `error:`/`note:`):
///   {pad}--> input:line:col
///   {pad} |
///   {line_no} | <source line>
///   {pad} | <indent>^^^
fn caret_block(source: &str, span: Span) -> String {
    let (line_idx, line_start) = line_at(source, span.start);
    let line_end = source[line_start..]
        .find('\n')
        .map(|n| line_start + n)
        .unwrap_or(source.len());
    let line_text = &source[line_start..line_end];
    let col = span.start - line_start;
    let underline_len = (span.end.min(line_end) - span.start).max(1);

    let line_no = line_idx + 1;
    let pad = line_no.to_string().len();
    let blank = " ".repeat(pad);
    format!(
        "{blank}--> input:{line_no}:{col_no}\n\
         {blank} |\n\
         {line_no} | {line_text}\n\
         {blank} | {caret_pad}{carets}",
        col_no = col + 1,
        caret_pad = " ".repeat(col),
        carets = "^".repeat(underline_len),
    )
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

fn line_at(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 0;
    let mut line_start = 0;
    for (i, ch) in source.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    (line, line_start)
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Function {
        pub name: String,
        /// Declared `pub`: the function may be imported by other files. A
        /// non-`pub` (private) function is usable only within its own file —
        /// importing it is a loader error. Always treated as public for the
        /// builtin pseudo-declarations and within a single file.
        pub is_pub: bool,
        /// Declared generic type parameters, e.g. `fn f<T: any>(...)` →
        /// `["T"]`. These names act as type variables in the signature
        /// (params + return) and are resolved by monomorphization.
        pub type_params: Vec<String>,
        pub params: Vec<Param>,
        /// Effects declared in the signature, e.g. `!prints`. Callers of this
        /// function must declare at least these effects themselves.
        pub effects: Vec<String>,
        pub return_ty: Option<Type>,
        pub body: Expr,
        /// Indices of parameters this (monomorphized) instance *takes ownership
        /// of*: the caller transfers its sole reference instead of retaining,
        /// and the callee is responsible for consuming it (so it isn't dropped
        /// on entry-scope exit). Set only by monomorphization, which emits a
        /// distinct owned instance when a call passes a fresh, uniquely-owned
        /// heap argument. Empty for source functions and borrow instances.
        pub owned_params: Vec<usize>,
        /// Indices of `str` parameters this (monomorphized) instance receives in
        /// the *concatenated-string* representation (a lazy concat node — see
        /// [`crate::CONCAT_STR`]). Set only by monomorphization, which emits a
        /// distinct concat-specialized instance (`$c{i}`) when a call passes a
        /// concat-typed argument to a `str` parameter. Empty for source functions
        /// and plain-`str` instances. The parameter's `ty` is retyped to the
        /// concat sentinel in such an instance, so codegen still sees a str-repr
        /// parameter; this list records *which* for repr-aware passes.
        pub concat_params: Vec<usize>,
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
    }

    /// The language's built-in scalar primitive types: the fixed-width integers
    /// (`i8`..`i64`, `u8`..`u64`), `bool`, `char`, and `str`. This is a *closed*
    /// set, so it's a proper enum rather than a stringly-typed name —
    /// `Type::Primitive(..)` is what used to be `Type::Named("i64")` and the
    /// like. (User structs, variants, generic type parameters, the builtin
    /// `Error` type, and the compiler's pseudo-type sentinels — `__none__`,
    /// `__unit__`, `any`, etc. — remain `Type::Named(String)`.)
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Type {
        /// A built-in scalar primitive (`i64`, `bool`, `str`, …). See
        /// [`Primitive`].
        Primitive(Primitive),
        /// A name that isn't a primitive: a user struct or variant, a generic
        /// type parameter (`T`), the builtin `Error` type, or a compiler
        /// pseudo-type sentinel (`__none__`, `__unit__`, `any`, …).
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
        /// `name = value; body` — store to an existing mut binding.
        Assign(String, Box<Expr>, Box<Expr>),
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
        /// element wrapped in `some` when in bounds, else `none`.
        Index(Box<Expr>, Box<Expr>),
        /// `receiver[start..end]` — string slicing (`recv`, `start`, `end`).
        /// Evaluates to a `str` holding the bytes in `[start, end)`, with both
        /// bounds clamped to `[0, len]` (out-of-range ends yield a shorter
        /// string; `start >= end` yields `""`). The result shares the source's
        /// backing buffer when possible (a copy for a small or SSO source).
        /// An open-ended `receiver[start..]` (end `None`) runs to the receiver's
        /// length — codegen fills it in, so no user-level `len` is needed.
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
/// `i64::MAX`, which fits `u64`).
pub fn int_fits(v: i64, name: &str) -> bool {
    match name {
        "i8" => i8::try_from(v).is_ok(),
        "i16" => i16::try_from(v).is_ok(),
        "i32" => i32::try_from(v).is_ok(),
        "i64" => true,
        "u8" => u8::try_from(v).is_ok(),
        "u16" => u16::try_from(v).is_ok(),
        "u32" => u32::try_from(v).is_ok(),
        "u64" => v >= 0,
        _ => false,
    }
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

/// If `name` is a named operator builtin, the operator it provides. An operator
/// builtin must be imported aliased to that operator: `import { wrapping_add as
/// + } from builtins;`. (Later additions like `saturating_add` will also map to
/// `+`, letting a file pick the `+` semantics it wants.)
pub fn operator_builtin(name: &str) -> Option<&'static str> {
    match name {
        "wrapping_add" => Some("+"),
        _ => None,
    }
}

/// Whether `s` spells a built-in operator that must be imported to be used
/// (e.g. `import { == } from builtins;`; `+` comes via `wrapping_add as +`).
pub fn is_operator_name(s: &str) -> bool {
    matches!(
        s,
        "+" | "-"
            | "*"
            | "/"
            | "%"
            | "=="
            | "!="
            | "<"
            | ">"
            | "<="
            | ">="
            | "&&"
            | "||"
            | "!"
            | "++"
    )
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
        K::Field(x, _) | K::Try(x) | K::Return(x) => collect_operators(x, out),
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

/// The *concatenated-string* representation of `str`: an internal, mono-only
/// pseudo-type that flows out of `a + b` (string concat) to mark a value built as
/// a lazy concat node (see `aipl_concat_lazy`). To the source author it is just a
/// `str` — it never appears in source and the standalone checker never sees it.
/// Its only role is in monomorphization: passing a concat-typed value to a
/// `fn(s: str)` selects a distinct, concat-specialized instance of that function
/// (the `$c{i}` instances), mirroring how `str_params`/`owned_params` specialize.
/// It has the `str` runtime representation (`is_str_repr` below), so all codegen
/// machinery treats it exactly like a `str`.
pub const CONCAT_STR: &str = "__concat_str__";

pub fn concat_str_ty() -> Type {
    Type::Named(CONCAT_STR.into())
}

pub fn is_concat_str(t: &Type) -> bool {
    matches!(t, Type::Named(s) if s == CONCAT_STR)
}

/// Whether `t` has the `str` runtime representation: `str` itself, the builtin
/// `Error` type (currently a string under the hood), or the internal concat-str
/// representation. These share all codegen machinery — refcounting, equality,
/// hashing, rendering.
pub fn is_str_repr(t: &Type) -> bool {
    matches!(t, Type::Primitive(Primitive::Str)) || is_error(t) || is_concat_str(t)
}

/// The unit type — what a function with no declared return type produces:
/// nothing. Spelled with a reserved name users can't write (omitting `-> ty`
/// is the only way to get it), so it behaves like the other pseudo-types
/// (`__none__`, the array/none markers). It never appears as a parameter,
/// field, array element, or optional inner — the grammar can't express it
/// there — so type validation treats it as an unknown scalar and rejects it
/// in those positions, leaving the function return as its only home.
pub const UNIT: &str = "__unit__";

pub fn unit_ty() -> Type {
    Type::Named(UNIT.into())
}

pub fn is_unit(t: &Type) -> bool {
    matches!(t, Type::Named(s) if s == UNIT)
}

pub fn type_name(t: &Type) -> String {
    match t {
        Type::Primitive(p) => p.name().into(),
        Type::Named(s) if s == EMPTY_ARRAY_ARG => "EmptyArray".into(),
        Type::Named(s) if s == NONE_LITERAL_ARG => "NoneLiteral".into(),
        Type::Named(s) if s == UNIT => "()".into(),
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
/// is the only way to spell it.
pub fn none_inner_ty() -> Type {
    Type::Named("__none__".into())
}

pub fn is_none_inner(t: &Type) -> bool {
    matches!(t, Type::Named(s) if s == "__none__")
}

/// Marker the monomorphizer binds a type variable to when the only
/// argument that could pin it is an empty array literal — the resulting
/// instance accepts the pseudo-type `EmptyArray` (substituted to
/// `Array(__none__)` so existing codegen treats it as an empty array).
pub const EMPTY_ARRAY_ARG: &str = "__empty_array_arg__";

/// Marker the monomorphizer binds a type variable to when the only
/// argument that could pin it is the bare `none` literal — the resulting
/// instance accepts the pseudo-type `NoneLiteral` (substituted to
/// `Optional(__none__)`).
pub const NONE_LITERAL_ARG: &str = "__none_literal_arg__";

pub fn empty_array_arg_ty() -> Type {
    Type::Named(EMPTY_ARRAY_ARG.into())
}

pub fn none_literal_arg_ty() -> Type {
    Type::Named(NONE_LITERAL_ARG.into())
}

pub fn is_empty_array_arg(t: &Type) -> bool {
    matches!(t, Type::Named(s) if s == EMPTY_ARRAY_ARG)
}

pub fn is_none_literal_arg(t: &Type) -> bool {
    matches!(t, Type::Named(s) if s == NONE_LITERAL_ARG)
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
    "value_or",
    "contains",
    "read_file_to_string",
    "write_string_to_file",
    "union",
    "get",
    "contains_key",
    "hash",
    "min",
    "max",
    "minimum",
    "maximum",
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
