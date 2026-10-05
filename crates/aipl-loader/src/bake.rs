//! The two source-text rewrites a file gets before its names are resolved:
//! `assert(cond)` and `trace(expr)`.
//!
//! Both are **reserved one-argument calls** — a name the language keeps for
//! itself, rewritten in place into a call to an intrinsic that carries the
//! argument's source location baked in as a string. Everything that
//! implementation needs is shared, which is why the two live in one file:
//!
//! | shared | what it is |
//! |---|---|
//! | [`Reserved`] | the name, what its one argument is, and what the intrinsic does — the three things both diagnostics need |
//! | [`reserved_names_not_bound`] | the refusal to let a file bind either name to something of its own |
//! | [`location_label`] | how a file's name reaches the baked string |
//! | [`Baker::location`] | the location string itself, from the dogfooded `source_loc` |
//! | [`rewrite_reserved_call`] | the walk that finds the calls, innermost first |
//!
//! # Why the loader
//!
//! A source location names a file and a line, and the parser is handed a source
//! with no name attached to it — `parse` takes a `&str`. The loader is the first
//! place that holds both, so it is where both rewrites happen. `assert`'s used to
//! live in the parser's `post_parse`, which is exactly why its locations used to
//! read `input:13:` — `input` was a placeholder standing in for the filename the
//! parser could not know.
//!
//! # The one place they differ
//!
//! Which bodies each applies to, and that difference *is* each feature: `trace`
//! is for debugging a program, so it is honoured wherever one is written, while
//! `assert` belongs to the test runner, so only test code may use it —
//! elsewhere it stays an ordinary unresolved call, which is what makes it
//! test-only. [`bake`] is where that is decided, and it is the only asymmetry.

use std::path::Path;

use aipl_syntax::ast::{Callee, Expr, ExprKind, ImportDecl, Item, Program};
use aipl_syntax::{Error, Span};

use crate::file_label;

/// A name the language reserves for a one-argument call it rewrites.
struct Reserved {
    /// The spelling a file writes.
    name: &'static str,
    /// Completes "`<name>` takes exactly one argument: <takes>" — the arity
    /// diagnostic.
    takes: &'static str,
    /// Completes "reserved for a compiler intrinsic (<does>)" — what the reader
    /// of [`reserved_names_not_bound`]'s diagnostic may not know the name is for.
    does: &'static str,
}

const ASSERT: Reserved = Reserved {
    name: "assert",
    takes: "the condition to check",
    does: "`assert(cond)` records a test failure against the line it is written on",
};

const TRACE: Reserved = Reserved {
    name: "trace",
    takes: "the expression to print",
    does: "`trace(expr)` prints the expression and its value, and yields it",
};

const RESERVED: &[&Reserved] = &[&ASSERT, &TRACE];

/// Reject a file that binds either reserved name to something of its own.
///
/// A reserved call is rewritten before any name resolution happens, so a
/// declaration of one of these names would not shadow the intrinsic — it would
/// simply never be reached, which is the kind of silence worth a diagnostic.
/// Covers the item-level bindings (a function, a constant, an import's local
/// name); a local `let trace = ..` used as a callable is beyond what the rewrite
/// can see.
pub(crate) fn reserved_names_not_bound(program: &Program) -> Result<(), Error> {
    // `a_binding` carries its own article: "a function", "an imported name".
    let reserved = |r: &Reserved, a_binding: &str, span: Span| {
        Error::at(
            format!(
                "`{}` is reserved for a compiler intrinsic ({}), so {a_binding} may not be \
                 called that: the call is rewritten before any name could shadow it, so the \
                 declaration would never be reached",
                r.name, r.does
            ),
            span,
        )
    };
    let which = |name: &str| RESERVED.iter().copied().find(|r| r.name == name);
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                if let Some(r) = which(&f.name.text) {
                    return Err(reserved(r, "a function", f.name.span.clone()));
                }
            }
            Item::Const(c) => {
                if let Some(r) = which(&c.name.text) {
                    return Err(reserved(r, "a constant", c.name.span.clone()));
                }
            }
            Item::Import(ImportDecl { names, .. }) => {
                for n in names {
                    if let Some(r) = which(n.local()) {
                        return Err(reserved(r, "an imported name", n.span.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// What a baked location names a file: `path` relative to the project root, with
/// forward slashes so the string reads the same on every platform.
///
/// A file outside the project root — or one loaded with no project root known,
/// which is every in-memory entry point — falls back to its bare name, the same
/// thing [`file_label`] shows a diagnostic about it. An absolute path would be
/// both noisy and particular to one machine.
pub(crate) fn location_label(path: &Path, project_root: Option<&Path>) -> String {
    // A virtual source's key *is* the name the caller gave it (`"./util.aipl"`),
    // so there is nothing to make it relative to.
    let rel = if path.is_relative() {
        path.strip_prefix(".").unwrap_or(path)
    } else {
        match project_root.and_then(|root| path.strip_prefix(root).ok()) {
            Some(rel) => rel,
            None => return file_label(path),
        }
    };
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Both rewrites over every body in `items`.
///
/// `src` must be the text `items`' spans index, and `label` the name the file
/// goes by ([`location_label`]).
pub(crate) fn bake(items: &mut [Item], src: &str, label: &str) -> Result<(), Error> {
    let mut baker = Baker {
        src,
        label,
        next_trace_value: 0,
    };
    for item in items {
        // A `.test` block's own functions were hoisted to top level by the
        // parser's `post_parse`, under a name `is_test_helper` recognizes — so a
        // function has exactly two bodies to walk, and test code is identifiable
        // by name.
        let Item::Fn(f) = item else { continue };
        let is_test_code = aipl_syntax::is_test_helper(&f.name);
        baker.bake_body(&mut f.body, is_test_code)?;
        if let Some(test_body) = &mut f.test_body {
            baker.bake_body(test_body, true)?;
        }
    }
    Ok(())
}

/// One file being rewritten: the text its spans index, the name it goes by, and
/// the serial that keeps the bindings [`Baker::trace_block`] synthesizes
/// distinct.
struct Baker<'a> {
    src: &'a str,
    label: &'a str,
    next_trace_value: u32,
}

impl Baker<'_> {
    /// `sub/lib.aipl:13 cond` — where `span` is, and what was written there.
    ///
    /// The line and the text come from the dogfooded `source_loc`; the label is
    /// this side's to prefix, because the file's name is what only the loader
    /// knows.
    fn location(&self, span: Span) -> String {
        format!("{}:{}", self.label, source_loc(self.src, span))
    }

    /// Both rewrites over one body. `is_test_code` is what admits `assert` — see
    /// the module docs.
    fn bake_body(&mut self, e: &mut Expr, is_test_code: bool) -> Result<(), Error> {
        if is_test_code {
            self.bake_asserts(e)?;
        }
        self.bake_traces(e)
    }

    /// Rewrite each `assert(cond)` into `__assert(cond, "sub/lib.aipl:13 cond")`,
    /// capturing the location the `check` failure report names.
    fn bake_asserts(&self, e: &mut Expr) -> Result<(), Error> {
        rewrite_reserved_call(e, &ASSERT, &mut |cond: Expr| {
            let loc = Expr::new(
                ExprKind::Str(self.location(cond.span.clone())),
                cond.span.clone(),
            );
            ExprKind::Call(Callee::Assert, vec![cond, loc], false)
        })
    }

    /// Rewrite each `trace(expr)` into the block it stands for.
    ///
    /// `trace(expr)` evaluates to `expr`, so that a trace can be wrapped around a
    /// subexpression in place rather than restructuring the code around it. The
    /// value is therefore bound once and handed back:
    ///
    /// ```text
    /// trace(v + 2)
    ///
    /// // becomes, for line 5 of subdir/file.aipl:
    /// let __trace_value_0 = v + 2;
    /// __trace(`subdir/file.aipl:5 v + 2 = {__trace_value_0}`);
    /// __trace_value_0
    /// ```
    fn bake_traces(&mut self, e: &mut Expr) -> Result<(), Error> {
        rewrite_reserved_call(e, &TRACE, &mut |traced: Expr| self.trace_block(traced))
    }

    /// The `let`/print/yield block one `trace(traced)` becomes — see
    /// [`Baker::bake_traces`].
    fn trace_block(&mut self, traced: Expr) -> ExprKind {
        let span = traced.span.clone();
        // Unique per file, and `__`-prefixed like every other synthesized
        // binding, so nested and sibling traces cannot collide with each other
        // or with a name from source.
        let value_name = format!("__trace_value_{}", self.next_trace_value);
        self.next_trace_value += 1;
        // Everything but the value, which the interpolation appends.
        let prefix = format!("{} = ", self.location(span.clone()));

        let at = |kind| Expr::new(kind, span.clone());
        let value = || at(ExprKind::Ident(value_name.clone()));
        let message = at(ExprKind::Call(
            Callee::TemplateConcat,
            vec![
                at(ExprKind::Str(prefix)),
                at(ExprKind::Call(Callee::TemplateInterp, vec![value()], false)),
            ],
            false,
        ));
        ExprKind::Let(
            value_name.clone(),
            None,
            Box::new(traced),
            Box::new(at(ExprKind::Seq(
                Box::new(at(ExprKind::Call(Callee::Trace, vec![message], false))),
                Box::new(value()),
            ))),
        )
    }
}

/// Rewrite every call to the reserved one-argument function `r` within `e`.
///
/// `rebuild` is handed the single argument — already rewritten itself, so a
/// nested use is resolved innermost-first — and returns what the call becomes.
/// Any other arity is the shared diagnostic below: a reserved name has exactly
/// one meaning, so there is nothing for a second argument to be.
///
/// The recursion is [`aipl_syntax::each_subexpr_mut`] rather than a match of its
/// own. A hand-rolled walk has to be kept in step with `ExprKind` by hand, and a
/// missing arm here is silent — a reserved call in that position simply would not
/// be rewritten, and would surface much later as an unresolved name.
fn rewrite_reserved_call(
    e: &mut Expr,
    r: &Reserved,
    rebuild: &mut impl FnMut(Expr) -> ExprKind,
) -> Result<(), Error> {
    if let ExprKind::Call(callee, args, _) = &e.kind {
        if *callee == r.name {
            if args.len() != 1 {
                return Err(Error::at(
                    format!(
                        "`{}` takes exactly one argument: {} (got {})",
                        r.name,
                        r.takes,
                        args.len()
                    ),
                    e.span.clone(),
                ));
            }
            let ExprKind::Call(_, mut args, _) = std::mem::replace(&mut e.kind, ExprKind::Unit)
            else {
                unreachable!("matched a call just above")
            };
            let mut arg = args.pop().expect("one argument");
            rewrite_reserved_call(&mut arg, r, rebuild)?;
            e.kind = rebuild(arg);
            return Ok(());
        }
    }
    for child in aipl_syntax::each_subexpr_mut(e) {
        rewrite_reserved_call(child, r, rebuild)?;
    }
    Ok(())
}

/// `LINE TEXT` for `span` within `src`. Dogfooded: the AIPL `source_loc`, run
/// through the embedding FFI via the installed hook. There is **no native
/// fallback** — it panics if the hook isn't installed, so install it (via
/// `install_parser_hooks`) before loading anything.
fn source_loc(src: &str, span: Span) -> String {
    let hook = SOURCE_LOC_HOOK
        .get()
        .expect("source-loc hook not installed before loading (call install_parser_hooks)");
    hook(src, span)
}

/// The source-location formatter, installed by the compiler (via
/// [`set_source_loc_hook`]) to dogfood the AIPL `source_loc`. Required — see
/// [`source_loc`].
static SOURCE_LOC_HOOK: std::sync::OnceLock<fn(&str, Span) -> String> = std::sync::OnceLock::new();

/// Install the source-location formatter. The compiler points this at the
/// dogfooded AIPL `source_loc`, run through the embedding FFI. First install wins
/// (the hook is process-global).
pub fn set_source_loc_hook(f: fn(&str, Span) -> String) {
    let _ = SOURCE_LOC_HOOK.set(f);
}
