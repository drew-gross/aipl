//! The optimization pass manager: run the passes until they stop finding
//! anything.
//!
//! Each pass on its own is a single sweep, and that is the problem it solves
//! badly: the passes feed each other. Inlining a call puts a `filter` and a
//! `map` next to each other, which is a chain fusion can collapse; fusion
//! leaves a call whose arguments are literals, which folding can evaluate;
//! folding turns a binding into a constant, which substitution can push into
//! its use site — and *that* can expose another chain. A pipeline that runs
//! each pass once stops wherever the last pass happened to leave off, and how
//! much it misses depends on nothing more principled than the order they were
//! written in.
//!
//! So the passes are a list here rather than a straight line of calls, and the
//! manager runs the list again whenever a round changed anything.
//!
//! # The worklist
//!
//! Re-running everything over everything would converge, and would pay for a
//! full sweep of the whole program per round to find the handful of functions
//! that are still moving. Instead each pass reports what it rewrote — by
//! comparing bodies, since a pass returns a new program rather than saying —
//! and the next round visits only those functions **and the functions that
//! call them** ([`Scope`]).
//!
//! The callers are the half that is easy to get wrong, and the reason the
//! distinction matters:
//!
//! - A **body-local** pass (folding, fusion, sinking, substitution) reads
//!   nothing outside the body it rewrites. Running it again on a body that did
//!   not change would produce that same body, so skipping it is not an
//!   approximation — it is the same answer, reached without the walk.
//! - An **inliner** reads its callees' bodies. A function whose callee just
//!   shrank may be inlinable where it was not, so a change to `g` makes every
//!   caller of `g` interesting again. That cascade is the whole point of
//!   iterating, and dropping it would make the fixpoint a false one.
//!
//! # What a pass may not assume
//!
//! A pass in this list can run more than once, which three of them care about:
//!
//! - [`Pass::once`] exists for a pass that must not. `inline_small` is the
//!   case: it duplicates a body at every call site, and a second round would
//!   inline into the copies the first one made — the "unbounded ping-pong
//!   between two small mutually-calling functions" its own comment refuses,
//!   now once per round. Its candidates can only shrink across rounds
//!   anyway, so running it again cannot find anything new that is not a copy
//!   of its own work.
//! - [`Pass::whole_program`] exists for a pass whose *selection* is global, so
//!   there is no per-function scope to honor. `inline_single_use` picks its
//!   candidate by whole-program use count and deletes the definition it
//!   inlined; a scope naming callers says nothing about which callee is
//!   single-use, and half-applying it would drop a definition whose one caller
//!   was skipped.
//! - Every other pass is [`Pass::scoped`] and takes the worklist.
//!
//! # Termination
//!
//! The scoped passes all shrink or normalize — folding replaces a tree with a
//! literal, substitution removes a binding, fusion replaces a chain with one
//! call, sinking moves a binding inward — so they run out of work. That is an
//! argument, not a proof, so there is a hard round cap: `rounds` bounds the
//! loop, and hitting it is traced rather than silent. Output stays
//! deterministic either way, because the cap is a constant rather than a
//! deadline.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{Callee, Expr, ExprKind, Item, Program};
use aipl_syntax::DebugOptions;

use crate::{ConcreteFn, MonoProgram};

/// Which functions a pass should rewrite this round.
#[derive(Debug, Clone)]
pub enum Scope {
    /// All of them — the first round, where nothing has been ruled out yet.
    Everything,
    /// Only these: what the previous round rewrote, plus the functions that
    /// call one of them.
    These(HashSet<String>),
}

impl Scope {
    /// Whether a pass should rewrite the function named `name`.
    pub fn covers(&self, name: &str) -> bool {
        match self {
            Scope::Everything => true,
            Scope::These(names) => names.contains(name),
        }
    }
}

/// One pass, as the manager sees it: a name to trace it by, a rewrite, and
/// whether it may run more than once.
pub struct Pass<P> {
    pub name: &'static str,
    rewrite: Box<dyn Fn(&P, &Scope) -> P>,
    first_round_only: bool,
}

impl<P> Pass<P> {
    /// A pass that rewrites the bodies the scope names and leaves the rest
    /// alone. The ordinary kind.
    pub fn scoped(name: &'static str, rewrite: impl Fn(&P, &Scope) -> P + 'static) -> Pass<P> {
        Pass {
            name,
            rewrite: Box::new(rewrite),
            first_round_only: false,
        }
    }

    /// A pass whose selection is whole-program, so it has no use for the
    /// scope. See the module docs — this is a statement about the pass, not a
    /// shortcut past the worklist.
    pub fn whole_program(name: &'static str, rewrite: impl Fn(&P) -> P + 'static) -> Pass<P> {
        Pass {
            name,
            rewrite: Box::new(move |program, _| rewrite(program)),
            first_round_only: false,
        }
    }

    /// A pass that must run exactly once, in the first round. See the module
    /// docs for the one case.
    pub fn once(self) -> Pass<P> {
        Pass {
            first_round_only: true,
            ..self
        }
    }
}

/// A program the manager can drive: it needs to see each function's name,
/// body, and the parameter names that shadow globals inside it. Nothing else —
/// the passes know what they are rewriting; the manager only has to tell what
/// moved.
pub trait Optimizable {
    fn functions(&self) -> Vec<FunctionView<'_>>;
}

/// What the manager needs of one function.
pub struct FunctionView<'a> {
    pub name: &'a str,
    pub body: &'a Expr,
    /// A `.test({ .. })` block's body, which is compiled like any other code
    /// and so is optimized like any other code — and has to be compared like
    /// it, or a pass that only improved a test would read as having done
    /// nothing.
    pub test_body: Option<&'a Expr>,
    /// Parameters shadow same-named globals within the body, so a call to a
    /// `(T) -> bool` parameter named `pred` is not a reference to a top-level
    /// `pred`. Without this the call graph gains edges that do not exist, and
    /// the worklist quietly grows back towards everything.
    pub params: Vec<String>,
}

impl Optimizable for Program {
    fn functions(&self) -> Vec<FunctionView<'_>> {
        self.items
            .iter()
            .filter_map(|item| match item {
                Item::Fn(f) => Some(FunctionView {
                    name: &f.name,
                    body: &f.body,
                    test_body: f.test_body.as_ref(),
                    params: f.sig.params.iter().map(|p| p.name.clone()).collect(),
                }),
                _ => None,
            })
            .collect()
    }
}

impl Optimizable for MonoProgram {
    fn functions(&self) -> Vec<FunctionView<'_>> {
        self.fns
            .iter()
            .map(|f: &ConcreteFn| FunctionView {
                name: &f.name,
                body: &f.body,
                // Monomorphization has already folded each `.test` block into
                // the `__test_main` it synthesizes, so there is no separate
                // test body left to compare by this point.
                test_body: None,
                params: f.params.iter().map(|p| p.name.clone()).collect(),
            })
            .collect()
    }
}

/// Run `passes` over `program` until a whole round changes nothing, or until
/// `rounds` rounds have been run.
///
/// Returns the optimized program. The pass order within a round is the order
/// given, and it is load-bearing — see the comments on the list each caller
/// builds.
pub fn optimize<P: Optimizable>(
    mut program: P,
    passes: &[Pass<P>],
    rounds: usize,
    dbg: DebugOptions,
) -> P {
    let mut scope = Scope::Everything;
    // Which spans arrive with a recorded type. Collected once: a pass may lose
    // a record but never adds one, so comparing each pass's output against the
    // original set is both cheaper and stricter than re-deriving it per pass.
    let recorded = match cfg!(debug_assertions) {
        true => recorded_spans(&program),
        false => HashSet::new(),
    };
    // Nothing may arrive already missing a record that the same span carries
    // elsewhere. This used to be an exclusion set rather than an assertion,
    // because it was not empty: monomorphization instantiates one template
    // into several functions that all keep the template's spans, and
    // `subst_expr_tys` rebuilt each instance's body with `ty: None` — so a
    // record the checker made survived on one instance and vanished on the
    // next. `walker.aipl` entered the post-mono passes with 365 recorded spans
    // and 1 already lost, `grammar_aipl.aipl` with 236 and 2. That is fixed at
    // the source now (`subst_expr_tys` substitutes the record through, like
    // every other annotation it carries), so this is an assertion: if it
    // fires, whatever built this program dropped a record before the passes
    // got it — for the post-mono phase, monomorphization again.
    //
    // Note when chasing one that a span is only meaningful *with its file*: a
    // monomorphized program merges many sources and every span is an offset
    // into whichever one its function came from, so reading one against the
    // file named on the command line lands anywhere.
    if cfg!(debug_assertions) {
        let inherited = lost_records(&program, &recorded);
        assert!(
            inherited.is_empty(),
            "the program handed to the pass manager is already missing the \
             recorded type of {} context-typed expression(s) — dropped before \
             any pass here ran, so by whatever produced it (monomorphization, \
             for the post-mono phase). Sites:\n  {}",
            inherited.len(),
            inherited.join("\n  "),
        );
    }

    for round in 1..=rounds {
        let mut changed: HashSet<String> = HashSet::new();
        for pass in passes {
            if pass.first_round_only && round > 1 {
                continue;
            }
            let next = (pass.rewrite)(&program, &scope);
            // Checked per pass, because this is the only point at which the
            // culprit is still known: one pass later and the only evidence is
            // a program that compiles to the wrong representation.
            if cfg!(debug_assertions) {
                let lost = lost_records(&next, &recorded);
                assert!(
                    lost.is_empty(),
                    "pass `{}` (round {round}) dropped the recorded type of {} \
                     context-typed expression(s). The checker resolved these from \
                     the expected type and wrote the answer onto the expression so \
                     that a pass could move them; rebuilding one with `Expr::new` \
                     instead of `Expr::rebuilt` loses it, and codegen then picks \
                     the representation from nothing. Sites:\n  {}",
                    pass.name,
                    lost.len(),
                    lost.join("\n  "),
                );
            }
            let rewritten = rewritten_functions(&program, &next);
            if !rewritten.is_empty() {
                dbg.trace(
                    "passes",
                    format_args!("round {round}: {} rewrote {}", pass.name, named(&rewritten)),
                );
            }
            changed.extend(rewritten);
            program = next;
        }
        if changed.is_empty() {
            dbg.trace(
                "passes",
                format_args!("fixpoint after {round} round(s) of {}", passes.len()),
            );
            return program;
        }
        scope = Scope::These(with_callers_of(&program, &changed));
    }
    // Reaching the cap is not wrong — the program is valid, just possibly
    // improvable — but it is worth knowing about, because the alternative
    // explanation is a pair of passes undoing each other every round.
    dbg.trace(
        "passes",
        format_args!("stopped at the {rounds}-round cap without reaching a fixpoint"),
    );
    program
}

/// Where a context-typed expression carries its recorded type, keyed by span.
///
/// The checker resolves these from the expected type and writes the answer onto
/// the expression ([`Expr::ty`]) precisely so that a later pass may move them.
/// `crate::check::needs_lock` is the candidate set, and its own doc notes it is
/// "deliberately the same set the inliner used to refuse to move". Codegen
/// reads the record back (`let locked = expr.ty...`) and for some of them picks
/// the *runtime representation* from it: an empty `#{}` is the shared empty
/// block while `#{char}` is a 256-bit bitfield, and `xs.to_set()` builds an
/// ordered set or an unordered one depending on what was recorded.
///
/// So a pass that rebuilds one of these without carrying `ty` across does not
/// produce a worse program — it produces a differently-typed one, and codegen
/// either picks the wrong representation or rejects a call the checker
/// accepted.
///
/// # Why spans, and why not a count
///
/// Being unrecorded is *normal*: the checker records a lock only where the
/// resolution is worth recording, so one real source file enters this with
/// ~190 unrecorded context-typed expressions. Counting them therefore proves
/// nothing — `inline_small` duplicates a body at every call site, so copying
/// one already-unrecorded `none` five times raises the count five times
/// without anything having been lost. It is a convincing false positive: it is
/// the first thing this check reported when it counted.
///
/// Span is the only identity that survives a pass. The checker's own `node_id`
/// is a pointer address, good for one walk and meaningless after a rebuild;
/// `Expr::rebuilt` and `rename_params` both carry the span across, and a
/// duplicated body's copies all keep the span they came from — so "this span
/// had its type recorded, and now an expression at that span does not" is
/// exactly the loss, and duplication cannot fake it.
fn recorded_spans(program: &impl Optimizable) -> HashSet<(usize, usize)> {
    let mut out = HashSet::new();
    for f in program.functions() {
        collect_recorded(f.body, &mut out);
        if let Some(test_body) = f.test_body {
            collect_recorded(test_body, &mut out);
        }
    }
    out
}

fn collect_recorded(e: &Expr, out: &mut HashSet<(usize, usize)>) {
    if is_context_typed(e) && e.ty.is_some() {
        out.insert((e.span.start, e.span.end));
    }
    for child in crate::children(e) {
        collect_recorded(child, out);
    }
}

/// Context-typed expressions that have lost a record `recorded` says their
/// span had, ignoring the spans in `inherited` (see the caller).
fn lost_records(program: &impl Optimizable, recorded: &HashSet<(usize, usize)>) -> Vec<String> {
    let mut out = Vec::new();
    for f in program.functions() {
        let mut found = Vec::new();
        collect_lost(f.body, recorded, &mut found);
        if let Some(test_body) = f.test_body {
            collect_lost(test_body, recorded, &mut found);
        }
        out.extend(
            found
                .into_iter()
                .map(|(what, span)| format!("{} — {what} at bytes {}..{}", f.name, span.0, span.1)),
        );
    }
    out.sort();
    out.dedup();
    out
}

fn collect_lost(
    e: &Expr,
    recorded: &HashSet<(usize, usize)>,
    out: &mut Vec<(&'static str, (usize, usize))>,
) {
    let span = (e.span.start, e.span.end);
    if is_context_typed(e) && e.ty.is_none() && recorded.contains(&span) {
        out.push((context_kind(e), span));
    }
    for child in crate::children(e) {
        collect_lost(child, recorded, out);
    }
}

/// Whether `e`'s type comes from where it sits rather than from what it says.
fn is_context_typed(e: &Expr) -> bool {
    crate::check::needs_lock(&e.kind) || aipl_syntax::ctor_ref_case(e).is_some()
}

/// How a context-typed expression reads in the panic message. Its kind and
/// span are what identify the site, together with the function it is in.
fn context_kind(e: &Expr) -> &'static str {
    match &e.kind {
        ExprKind::None => "a bare `none`",
        ExprKind::ArrayLit(_) => "an empty `[]`",
        ExprKind::SetLit(..) => "an empty set literal",
        ExprKind::DictLit(_) => "an empty dict literal",
        ExprKind::Call(callee, ..) => match callee {
            Callee::Ok => "an `ok(..)`",
            Callee::Err => "an `err(..)`",
            Callee::Some => "a `some(..)`",
            Callee::ToSet => "a `to_set()`",
            _ => "a call",
        },
        _ => "a constructor reference",
    }
}

/// A rewritten set as the trace names it: the functions themselves while there
/// are few enough to read, and a count past that. Naming them is what turns
/// "something is still moving" into "this function is still moving", which is
/// the only way to tell slow convergence from two passes undoing each other.
fn named(functions: &HashSet<String>) -> String {
    const MANY: usize = 4;
    let mut names: Vec<&str> = functions.iter().map(String::as_str).collect();
    names.sort_unstable();
    match names.len() {
        n if n <= MANY => format!("{} fn(s): {}", n, names.join(", ")),
        n => format!("{n} fn(s): {}, …", names[..MANY].join(", ")),
    }
}

/// The functions `after` holds that `before` did not hold identically: the ones
/// a pass rewrote, and any it introduced.
///
/// A function that *disappeared* is not reported. It disappears by being
/// inlined into its one caller, and that caller's body changed — so the
/// progress is already accounted for, by the function that absorbed it.
fn rewritten_functions<P: Optimizable>(before: &P, after: &P) -> HashSet<String> {
    let was: HashMap<&str, (&Expr, Option<&Expr>)> = before
        .functions()
        .into_iter()
        .map(|f| (f.name, (f.body, f.test_body)))
        .collect();
    after
        .functions()
        .into_iter()
        .filter(|f| was.get(f.name) != Some(&(f.body, f.test_body)))
        .map(|f| f.name.to_string())
        .collect()
}

/// `changed`, plus every function that refers to something in `changed` — the
/// next round's worklist. See the module docs for why the callers belong.
fn with_callers_of<P: Optimizable>(program: &P, changed: &HashSet<String>) -> HashSet<String> {
    let mut pending = changed.clone();
    for f in program.functions() {
        if references(&f).iter().any(|name| changed.contains(name)) {
            pending.insert(f.name.to_string());
        }
    }
    pending
}

/// Every function name one function's body refers to, by call or as a value.
///
/// Asked of [`crate::count_uses`] rather than of a walk written here, so that
/// what counts as a reference — and which names a parameter or a `let` shadows
/// — is decided in exactly one place. The counts themselves are not wanted;
/// only which names appeared.
fn references(f: &FunctionView<'_>) -> HashSet<String> {
    let mut bound: HashSet<String> = f.params.iter().cloned().collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    crate::count_uses(f.body, &mut bound, &mut counts);
    if let Some(test_body) = f.test_body {
        crate::count_uses(test_body, &mut bound, &mut counts);
    }
    counts.into_keys().collect()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use aipl_syntax::ast::{Callee, ExprKind, Function, Signature};
    use aipl_syntax::SpanStr;

    use super::*;

    /// A function with the given name and body, and nothing else — the manager
    /// looks at the name, the body and the parameter names, so the rest of a
    /// declaration is noise here.
    fn function(name: &str, body: Expr) -> Item {
        Item::Fn(Function {
            name: SpanStr::synthetic(name.to_string()),
            is_pub: false,
            sig: Signature {
                type_vars: Vec::new(),
                params: Vec::new(),
                effects: Vec::new(),
                return_ty: None,
            },
            body,
            test_body: None,
            test_fns: Vec::new(),
            doc: None,
        })
    }

    fn number(n: i64) -> Expr {
        Expr::new(ExprKind::Num(n), 0..0)
    }

    /// A body that calls `callee` — an edge in the call graph, which is what
    /// the worklist follows.
    fn call(callee: &str) -> Expr {
        Expr::new(
            ExprKind::Call(Callee::User(callee.to_string()), Vec::new(), false),
            0..0,
        )
    }

    fn program(fns: Vec<Item>) -> Program {
        Program {
            items: fns,
            sources: Vec::new(),
            doc: None,
        }
    }

    fn literal(program: &Program, name: &str) -> i64 {
        let f = program
            .functions()
            .into_iter()
            .find(|f| f.name == name)
            .expect("the function");
        match &f.body.kind {
            ExprKind::Num(n) => *n,
            other => panic!("not a literal: {other:?}"),
        }
    }

    /// A pass that adds one to every numeric body the scope covers, stopping
    /// at `stop`. It stands in for a real pass in the one way that matters
    /// here: it makes progress for a while, then stops, and it never says so —
    /// the manager works that out by looking at what came back.
    fn increment_to(stop: i64) -> Pass<Program> {
        Pass::scoped("increment", move |program: &Program, scope: &Scope| {
            let mut next = program.clone();
            for item in &mut next.items {
                if let Item::Fn(f) = item {
                    if !scope.covers(&f.name) {
                        continue;
                    }
                    if let ExprKind::Num(n) = &mut f.body.kind {
                        if *n < stop {
                            *n += 1;
                        }
                    }
                }
            }
            next
        })
    }

    #[test]
    fn iterates_until_a_round_changes_nothing() {
        let out = optimize(
            program(vec![function("a", number(0))]),
            &[increment_to(5)],
            100,
            DebugOptions::new(false),
        );
        assert_eq!(literal(&out, "a"), 5, "one round per increment, then stop");
    }

    #[test]
    fn stops_at_the_round_cap() {
        let out = optimize(
            program(vec![function("a", number(0))]),
            &[increment_to(1000)],
            3,
            DebugOptions::new(false),
        );
        assert_eq!(literal(&out, "a"), 3, "three rounds, three increments");
    }

    /// A `none` whose type the checker resolved and recorded, at a span.
    fn pinned_none(span: std::ops::Range<usize>) -> Expr {
        Expr::new(ExprKind::None, span).with_ty(aipl_syntax::ast::Type::Optional(Box::new(
            aipl_syntax::ast::Type::Primitive(aipl_syntax::ast::Primitive::I64),
        )))
    }

    /// Rebuilding a context-typed expression with `Expr::new` loses the
    /// recorded type, which is the bug the guard exists for.
    #[test]
    #[should_panic(expected = "dropped the recorded type")]
    fn a_pass_that_drops_a_recorded_type_is_caught() {
        let dropper = Pass::scoped("dropper", |program: &Program, _| {
            let mut next = program.clone();
            for item in &mut next.items {
                if let Item::Fn(f) = item {
                    f.body = Expr::new(f.body.kind.clone(), f.body.span.clone());
                }
            }
            next
        });
        optimize(
            program(vec![function("f", pinned_none(10..14))]),
            &[dropper],
            100,
            DebugOptions::new(false),
        );
    }

    /// The same rewrite through `Expr::rebuilt`, which carries `ty` across, is
    /// not a violation — otherwise the guard would refuse every pass.
    #[test]
    fn rebuilding_through_rebuilt_is_fine() {
        let keeper = Pass::scoped("keeper", |program: &Program, _| {
            let mut next = program.clone();
            for item in &mut next.items {
                if let Item::Fn(f) = item {
                    f.body = Expr::rebuilt(f.body.kind.clone(), &f.body);
                }
            }
            next
        });
        let out = optimize(
            program(vec![function("f", pinned_none(10..14))]),
            &[keeper],
            100,
            DebugOptions::new(false),
        );
        assert!(out.functions()[0].body.ty.is_some(), "the record survived");
    }

    /// Duplicating a body that was *already* unrecorded is not a loss — the
    /// false positive that counting unrecorded sites produced, and the reason
    /// the guard keys on spans instead.
    #[test]
    fn copying_an_unrecorded_expression_is_not_a_loss() {
        let bare = Expr::new(ExprKind::None, 10..14);
        let duplicator = Pass::scoped("duplicator", |program: &Program, _| {
            let mut next = program.clone();
            // One `none` becomes two, neither recorded, as inlining a body at
            // two call sites would.
            for item in &mut next.items {
                if let Item::Fn(f) = item {
                    f.body = Expr::new(
                        ExprKind::Seq(Box::new(f.body.clone()), Box::new(f.body.clone())),
                        f.body.span.clone(),
                    );
                }
            }
            next
        });
        optimize(
            program(vec![function("f", bare)]),
            &[duplicator],
            2,
            DebugOptions::new(false),
        );
    }

    /// The worklist, which is the whole reason this is a manager and not a
    /// loop: after the first round, a pass is asked only about the functions
    /// that moved and the functions that call them.
    #[test]
    fn a_later_round_visits_what_moved_and_who_calls_it() {
        // `caller` calls `a`; `bystander` calls nobody. Only `a` ever changes.
        let source = program(vec![
            function("a", number(0)),
            function("bystander", number(99)),
            function("caller", call("a")),
        ]);

        let asked: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let recorder = Rc::clone(&asked);
        let pass = Pass::scoped("increment-a", move |program: &Program, scope: &Scope| {
            let mut round = Vec::new();
            let mut next = program.clone();
            for item in &mut next.items {
                if let Item::Fn(f) = item {
                    if !scope.covers(&f.name) {
                        continue;
                    }
                    round.push(f.name.text.clone());
                    if f.name == "a" {
                        if let ExprKind::Num(n) = &mut f.body.kind {
                            if *n < 2 {
                                *n += 1;
                            }
                        }
                    }
                }
            }
            recorder.borrow_mut().push(round);
            next
        });

        optimize(source, &[pass], 100, DebugOptions::new(false));

        let asked = asked.borrow();
        assert_eq!(
            asked[0],
            vec!["a", "bystander", "caller"],
            "the first round knows nothing, so it asks about everything"
        );
        for (round, names) in asked.iter().enumerate().skip(1) {
            assert_eq!(
                names,
                &vec!["a".to_string(), "caller".to_string()],
                "round {} should have dropped the bystander",
                round + 1
            );
        }
        assert!(asked.len() >= 3, "it kept going while `a` moved");
    }

    /// A `once` pass runs in the first round and never again, however many
    /// rounds the rest of the list earns.
    #[test]
    fn a_once_pass_runs_only_in_the_first_round() {
        let out = optimize(
            program(vec![function("a", number(0))]),
            &[increment_to(100).once(), increment_to(4)],
            100,
            DebugOptions::new(false),
        );
        // Round 1 fires both passes (+2); later rounds only the second, which
        // stops at 4.
        assert_eq!(literal(&out, "a"), 4);
    }

    /// A pass whose selection is whole-program is handed the program and not
    /// the scope, so it rewrites what it likes in every round.
    #[test]
    fn a_whole_program_pass_is_not_given_a_scope() {
        let pass = Pass::whole_program("retire", |program: &Program| {
            // Drop a function outright, the way `inline_single_use` does once
            // it has moved the body into its caller.
            let mut next = program.clone();
            next.items
                .retain(|item| !matches!(item, Item::Fn(f) if f.name == "gone"));
            next
        });
        let out = optimize(
            program(vec![
                function("kept", number(1)),
                function("gone", number(2)),
            ]),
            &[pass],
            100,
            DebugOptions::new(false),
        );
        let names: Vec<&str> = out.functions().iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["kept"]);
    }

    /// A function that disappeared is not itself progress — whoever absorbed
    /// it changed, and that is what keeps the loop going. Without this, a pass
    /// that only ever deletes would spin until the cap.
    #[test]
    fn a_deletion_alone_does_not_keep_the_loop_running() {
        let rounds: Rc<RefCell<usize>> = Rc::new(RefCell::new(0));
        let counter = Rc::clone(&rounds);
        let pass = Pass::whole_program("retire-one", move |program: &Program| {
            *counter.borrow_mut() += 1;
            let mut next = program.clone();
            next.items
                .retain(|item| !matches!(item, Item::Fn(f) if f.name == "gone"));
            next
        });
        optimize(
            program(vec![
                function("kept", number(1)),
                function("gone", number(2)),
            ]),
            &[pass],
            100,
            DebugOptions::new(false),
        );
        assert_eq!(*rounds.borrow(), 1, "one round, then nothing had changed");
    }
}
