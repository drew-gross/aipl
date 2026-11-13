//! Known-constructor elimination: an eliminator meeting the constructor it
//! takes apart, at every leaf of what it examines.
//!
//! The shape this exists for is `x.value_or_err(e)?`, which inlines to
//! `match (x) { some(v) => ok(v), none => err(e) }?`. Compiled as written, the
//! `match` builds a result value — a `{tag, payload}` on the stack — in *both*
//! arms, and the `?` then reads the tag straight back and copies the payload out
//! again: a construction and a test that exist only to hand a value from one arm
//! to the line after the `match`. The hand-written form this replaced,
//! `match (x) { some(v) => v, none => { return err(e); } }`, never built that
//! value, which is why the rewrite to `value_or_err` cost ~100 bytes per site
//! until this pass.
//!
//! The fix is the same for every such pair: move the eliminator to where the
//! answer is decided. Each *eliminator* is pushed through everything that
//! merely carries its operand's value (a `let`/`mut`/`set` chain, a `;`
//! sequence, an `if`, a `match`) to the leaves, and where a leaf is a *known
//! constructor* the pair cancels. Four eliminators, four constructor
//! families:
//!
//! - `?` over the optional and result constructors: `ok(v)?` and `some(v)?`
//!   are `v` — nothing to test, the tag is a literal — while `err(e)?` and
//!   `none?` stay as the early return `?` always was, now taken from inside
//!   the branch (codegen knows a literal under `?` and emits no test for it
//!   either).
//! - `match` over the optional and result constructors and over a variant's
//!   cases: `match (some(p)) { some(v) => body, .. }` is `let v = p; body`,
//!   `match (Pair(a, b)) { Pair(x, y) => body, .. }` is
//!   `let x = a; let y = b; body` — the arm the leaf selects, with its binders
//!   bound to the leaf's payloads in order.
//! - Field access over a struct construction: `(S { a: x, b: y }).a` is `x`.
//!   The other fields are never built, so the retain the construction gave
//!   each of them and the release the struct's drop paid back are both gone
//!   — unless a field does something (an effectful call, an `assert`), in
//!   which case it is kept as a statement ahead of the projected value, in
//!   its original order.
//! - `==`/`!=` with a constructor on one side: `x == some(y)` reads as "is
//!   `x` this optional", and compiled as written an optional is built out of
//!   `y` — a stack slot, a tag store, a payload store, for a heap payload a
//!   retain — to be compared field by field and thrown away. Half of it is a
//!   tag the comparison already knows, so instead the tag is asked directly:
//!   `match (x) { some(v) => v == y, none => false }`, and for a variant
//!   `match (x) { Pair(v0, v1) => v0 == a && v1 == b, _ => false }` (`!=`
//!   inverts both: `!=`/`||` in the hit arm, `true` in the miss). The other
//!   operand moves into the arm only when evaluating it cannot be observed
//!   (the sinker's own test, [`can_defer`]) — `x == some(f())` where `f`
//!   prints must print whether or not `x` is `none`, so that one is bound
//!   ahead of the `match` instead, where [`crate::sink_bindings_post_mono`]
//!   decides its fate the same way it does for every other binding. The
//!   `match` this leaves is then an eliminator like any other: it takes the
//!   second rule if `x` is itself a branch of constructors, and a payload
//!   moved into the arm that is itself a constructor (`a == Cons(1, Cons(2,
//!   Nil))`) lands in a comparison that is unwrapped in turn. Measured on a
//!   bare `i64?`: 274 instructions down to 211, 1728 bytes down to 1600; a
//!   heap payload saves its retain/release on top.
//!
//! For that to be a rewrite of the *whole* expression, every branch the
//! eliminator reaches has to end in a constructor it knows: then each leaf
//! yields the payload, leaves, or runs its arm, and the wrapper is gone. A
//! branch that ends in anything else (a call returning a result, a binding
//! holding one) blocks the push for the whole expression — the eliminator stays
//! where it was, since pushing it down there would only duplicate the test at
//! every leaf.
//!
//! Pushing a `match` duplicates its arms: an arm is copied to every leaf that
//! selects it. `?` and a field access have nothing to copy, but a `match` is
//! admitted only while the copies stay small (`max_duplicated`, the same
//! threshold that admits a small function to inlining), and never when a
//! binding the scrutinee introduces would capture a name an arm reads — the
//! arm would move into that binding's scope, and inlined bodies keep their own
//! `let` names. A leaf whose payloads read one of the arm's own binders is
//! refused for the same reason: the binders are bound one after another, so a
//! later payload would see an earlier binder instead of the name it meant.
//!
//! A `match` whose scrutinee is a *name* bound just above it is the same
//! shape one step removed — and it is the shape inlining leaves behind, since
//! an inlined call binds each argument to a `let` ahead of the body:
//! `opt.map(f).value_or(d)` arrives as `let self: U? = <map's match>;
//! match (self) { .. }`. When that name is read exactly once, there, and the
//! value may move to where it is read (the sinker's [`can_defer`], with
//! nothing in between writing what the value reads), the `match` is pushed
//! into the value directly and the binding disappears. The binding's
//! annotation is not lost with it: it is the scrutinee's type, so each payload
//! binder the push introduces is annotated with the side it takes — which is
//! what keeps a leaf like `some(none)`, whose payload has no type of its own,
//! typed.
//!
//! The walk never enters a loop, a lambda, or a shim body, whose value is not
//! the tail's. A comparison with a constructor on *both* sides is left alone:
//! that is two literals, which is constant folding's to settle, and the
//! rewrite would only turn it into a match on a value that is right there.
//!
//! Post-mono only: the shapes are created by inlining — the
//! AIPL-implemented builtins (`value_or_err`, `value_or`, `is_some_and`,
//! `map_ok`, `map_err`, `try_map`), which mono instantiates and the post-mono
//! inliner folds into their callers, and any small function that builds a
//! struct or variant its caller immediately takes apart. By then `if let` has
//! been lowered to `match`, every pattern is a constructor, a binding, or a
//! wildcard, and an ignored payload is a `_` binder per slot. A user's own
//! `f(x)?` where `f`'s body ends in `ok(..)` and was inlined pre-mono is caught
//! at the same time, since one walk sees the whole program.

use crate::passes::Scope;
use std::collections::HashSet;

use aipl_syntax::ast::ConcreteType;
use aipl_syntax::typed::{TExpr, TExprKind, TFieldInit, TMatchArm, TPattern};

use crate::typed_util::Typed;

use crate::sink::undeferrable_fns_post_mono;
use crate::typed_util::{
    any_child, assigned_names, body_size, contains_context_literal, distinct_binders,
    each_child_mut, read_names, rename_scoped, uses_of,
};
use aipl_syntax::ast::Callee;

type Arm = TMatchArm<ConcreteType>;
type Pattern = TPattern<ConcreteType>;

/// Eliminate every known-constructor pair in `program`. `max_duplicated` bounds
/// how much arm text a pushed `match` may copy; `builtin_effects` is the effect
/// declarations under their pre-mono names (the set
/// [`crate::sink_bindings_post_mono`] takes), which decides whether a field a
/// projection discards can be dropped or must be kept for what it does. See
/// the module docs.
/// `scope` names the functions to rewrite: the pass is body-local, so a
/// function the scope leaves out would come back unchanged anyway (see
/// [`crate::passes`]), and skipping it saves the walk rather than changing the
/// answer.
pub fn eliminate_known_constructors_post_mono(
    program: &crate::MonoProgram,
    max_duplicated: usize,
    builtin_effects: &HashSet<String>,
    scope: &Scope,
) -> crate::MonoProgram {
    let limits = Limits {
        max_duplicated,
        blocked: undeferrable_fns_post_mono(program, builtin_effects),
    };
    crate::typed_util::map_bodies(
        program,
        |name| scope.covers(name),
        |_, body| *body = rewrite(body, &limits),
    )
}

/// What holds a push back: the arm-copy budget, and the functions whose call
/// cannot be dropped or deferred (see [`can_defer`]).
struct Limits {
    max_duplicated: usize,
    blocked: HashSet<String>,
}

/// Whether `value` may be moved into a branch — see [`crate::sink`].
fn can_defer(value: &Typed, blocked: &HashSet<String>) -> bool {
    crate::sink::can_defer_typed(value, blocked)
}

/// A node of `kind` and `ty` standing where `like` stood.
fn node(kind: TExprKind<ConcreteType>, ty: ConcreteType, like: &Typed) -> Typed {
    TExpr::rebuilt(kind, ty, like)
}

/// What the `let` node `e` declares its value as, when it declares anything.
fn declared(e: &Typed) -> Option<&ConcreteType> {
    match &e.kind {
        TExprKind::Let(..) | TExprKind::LetMut(..) => e.annotation.as_ref(),
        _ => None,
    }
}

/// Rebuild `e` with its children rewritten first, then rewrite `e` itself.
/// Bottom-up, so an eliminator whose operand only took the shape by an inner
/// rewrite (an `ok(..)?` inside an arm, a projection of a projection) is
/// pushed too.
fn rewrite(e: &Typed, limits: &Limits) -> Typed {
    let mut out = e.clone();
    each_child_mut(&mut out, &mut |child| *child = rewrite(child, limits));
    match &out.kind {
        TExprKind::Try(inner) if ends_in(inner, is_optional_or_result) => push_try(inner, &out),
        TExprKind::Match(scrutinee, arms)
            if ends_in(scrutinee, is_matchable)
                && match_can_move(scrutinee, arms, limits, false) =>
        {
            push_match(scrutinee, arms, &out, &|_| None)
        }
        TExprKind::Let(name, value, body) if ends_in(value, is_matchable) => {
            match_through_binding(name, declared(&out), value, body, limits).unwrap_or(out)
        }
        TExprKind::Field(base, field)
            if ends_in(base, is_struct) && field_can_move(base, field, limits) =>
        {
            push_field(base, field, &out, limits)
        }
        TExprKind::Call {
            callee: op @ (Callee::Equal | Callee::NotEqual),
            args,
            ..
        } if args.len() == 2 => {
            let sides = (
                known(&args[0]).filter(is_matchable),
                known(&args[1]).filter(is_matchable),
            );
            let (other, constructor) = match sides {
                (None, Some(k)) => (&args[0], k),
                (Some(k), None) => (&args[1], k),
                _ => return out,
            };
            let unwrapped = unwrap_equality(*op == Callee::Equal, other, constructor, &out, limits);
            // The `match` this built is an eliminator in its own right: give it
            // the same chance the walk gives one the program wrote.
            rewrite(&unwrapped, limits)
        }
        _ => out,
    }
}

/// `other == Case(p0, p1, ..)` (or `!=`, with `equal` false) as a `match` on
/// `other` that compares payloads in the arm the constructor's case selects and
/// answers the miss in the other: `none`/`some`, `err`/`ok`, or a catch-all
/// for every other case of a variant. A payload that does something is bound
/// ahead of the `match` (see the module docs), so `other` is still evaluated
/// first and the payloads after it, as the comparison did; one that only
/// computes a value goes straight into the arm — where, if it is itself a
/// constructor, the comparison it lands in is unwrapped in turn.
fn unwrap_equality(
    equal: bool,
    other: &Typed,
    constructor: Known<'_>,
    at: &Typed,
    limits: &Limits,
) -> Typed {
    let id = aipl_syntax::fresh_id();
    // Everything built here is a `bool` but the payload names.
    let boolean = at.ty.clone();
    let rebuilt = |kind: TExprKind<ConcreteType>| node(kind, boolean.clone(), at);
    // Each payload as the arm reads it: itself when it may be deferred, else
    // the name it is bound to ahead of the match.
    let payloads = constructor.payloads();
    let operands: Vec<(Typed, Option<Typed>)> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if can_defer(p, &limits.blocked) {
                ((*p).clone(), None)
            } else {
                let name = format!("$ceq{id}_{i}");
                (
                    node(TExprKind::Ident(name), p.ty.clone(), p),
                    Some((*p).clone()),
                )
            }
        })
        .collect();
    let binders: Vec<String> = (0..payloads.len())
        .map(|i| format!("$ceqv{id}_{i}"))
        .collect();
    // `v0 == p0 && v1 == p1 && ..` — or, for `!=`, `v0 != p0 || v1 != p1 || ..`.
    let (compare, join) = if equal {
        (Callee::Equal, Callee::LogicalAnd)
    } else {
        (Callee::NotEqual, Callee::LogicalOr)
    };
    let call = |callee: Callee, args: Vec<Typed>| {
        rebuilt(TExprKind::Call {
            callee,
            type_args: Vec::new(),
            args,
            method_style: false,
        })
    };
    let hit = binders
        .iter()
        .zip(&operands)
        .map(|(binder, (operand, _))| {
            call(
                compare.clone(),
                vec![
                    node(TExprKind::Ident(binder.clone()), operand.ty.clone(), at),
                    operand.clone(),
                ],
            )
        })
        .reduce(|acc, next| call(join.clone(), vec![acc, next]))
        .unwrap_or_else(|| rebuilt(TExprKind::Bool(equal)));
    let (hit_name, miss_pattern) = match &constructor {
        Known::Some(_) => ("some".to_string(), constructor_pattern("none")),
        Known::None => ("none".to_string(), constructor_pattern("some")),
        Known::Ok(_) => ("ok".to_string(), constructor_pattern("err")),
        Known::Err(_) => ("err".to_string(), constructor_pattern("ok")),
        Known::Variant { name, .. } => ((*name).to_string(), Pattern::Wildcard),
        Known::Struct(_) => unreachable!("a struct is not matchable"),
    };
    let arms = vec![
        Arm {
            pattern: Pattern::Ctor {
                name: hit_name,
                bindings: binders,
                ignore_payload: false,
            },
            body: hit,
            span: at.span.clone(),
        },
        Arm {
            pattern: miss_pattern,
            body: rebuilt(TExprKind::Bool(!equal)),
            span: at.span.clone(),
        },
    ];
    let matched = rebuilt(TExprKind::Match(Box::new(other.clone()), arms));
    // The bindings, outermost first, so the payloads run in their written order.
    operands
        .into_iter()
        .rev()
        .fold(matched, |body, (operand, bound)| match bound {
            Some(value) => {
                let TExprKind::Ident(name) = operand.kind else {
                    unreachable!("a bound payload is read through its name")
                };
                rebuilt(TExprKind::Let(name, Box::new(value), Box::new(body)))
            }
            None => body,
        })
}

/// The pattern that matches `case` and binds nothing — the miss arm of an
/// unwrapped comparison, which never looks at the payload (and, unbound,
/// never retains it).
fn constructor_pattern(case: &str) -> Pattern {
    Pattern::Ctor {
        name: case.to_string(),
        bindings: Vec::new(),
        ignore_payload: false,
    }
}

/// The constructor a leaf is, if it is one this pass knows how to take apart.
enum Known<'a> {
    /// `ok(v)` or `ok()`.
    Ok(Option<&'a Typed>),
    /// `err(e)`.
    Err(&'a Typed),
    /// `some(v)`.
    Some(&'a Typed),
    /// The `none` literal.
    None,
    /// A variant case, `Case(a, b)` or the nullary `Case`, named as the loader
    /// qualifies it (`Case@Variant`).
    Variant {
        name: &'a str,
        payloads: Vec<&'a Typed>,
    },
    /// A struct construction, `S { a: x, b: y }`.
    Struct(&'a [TFieldInit<ConcreteType>]),
}

/// The constructor families each eliminator takes apart.
fn is_optional_or_result(k: &Known<'_>) -> bool {
    matches!(
        k,
        Known::Ok(_) | Known::Err(_) | Known::Some(_) | Known::None
    )
}

fn is_matchable(k: &Known<'_>) -> bool {
    is_optional_or_result(k) || matches!(k, Known::Variant { .. })
}

fn is_struct(k: &Known<'_>) -> bool {
    matches!(k, Known::Struct(_))
}

impl Known<'_> {
    /// The bare case name a [`Pattern::Ctor`] matches this leaf by — for a
    /// variant case, without its `@Variant` qualifier, which the scrutinee's
    /// type already fixes (codegen compares the same way).
    fn case_name(&self) -> &str {
        match self {
            Known::Ok(_) => "ok",
            Known::Err(_) => "err",
            Known::Some(_) => "some",
            Known::None => "none",
            Known::Variant { name, .. } => name.split('@').next().unwrap_or(name),
            Known::Struct(_) => "",
        }
    }

    /// The payload expressions, in slot order.
    fn payloads(&self) -> Vec<&Typed> {
        match self {
            Known::Ok(p) => p.iter().copied().collect(),
            Known::Err(p) | Known::Some(p) => vec![p],
            Known::None => Vec::new(),
            Known::Variant { payloads, .. } => payloads.clone(),
            Known::Struct(fields) => fields.iter().map(|f| &f.value).collect(),
        }
    }
}

/// Whether `name` is a variant constructor reference: the loader qualifies
/// every one as `Case@Variant`, and nothing else can contain an `@`.
fn is_variant_constructor(name: &str) -> bool {
    name.contains('@')
}

fn known(e: &Typed) -> Option<Known<'_>> {
    Some(match &e.kind {
        TExprKind::Call {
            callee: Callee::Ok,
            args,
            ..
        } => match &args[..] {
            [] => Known::Ok(None),
            [v] => Known::Ok(Some(v)),
            _ => return None,
        },
        TExprKind::Call {
            callee: Callee::Err,
            args,
            ..
        } => match &args[..] {
            [v] => Known::Err(v),
            _ => return None,
        },
        TExprKind::Call {
            callee: Callee::Some,
            args,
            ..
        } => match &args[..] {
            [v] => Known::Some(v),
            _ => return None,
        },
        TExprKind::None => Known::None,
        TExprKind::Call {
            callee: Callee::User(name),
            args,
            method_style: false,
            ..
        } if is_variant_constructor(name) => Known::Variant {
            name,
            payloads: args.iter().collect(),
        },
        TExprKind::Ident(name) if is_variant_constructor(name) => Known::Variant {
            name,
            payloads: Vec::new(),
        },
        TExprKind::Construct(_, fields) => Known::Struct(fields),
        _ => return None,
    })
}

/// Whether every branch of `e` ends in a constructor `accepts` — the condition
/// under which pushing an eliminator through it removes the constructed value
/// rather than duplicating the test.
fn ends_in(e: &Typed, accepts: fn(&Known<'_>) -> bool) -> bool {
    match &e.kind {
        TExprKind::Let(_, _, body)
        | TExprKind::LetMut(_, _, body)
        | TExprKind::Assign(_, _, body)
        | TExprKind::Seq(_, body) => ends_in(body, accepts),
        TExprKind::If(_, t, f) => ends_in(t, accepts) && ends_in(f, accepts),
        TExprKind::Match(_, arms) => arms.iter().all(|a| ends_in(&a.body, accepts)),
        _ => known(e).is_some_and(|k| accepts(&k)),
    }
}

/// The leaves of `e` (which [`ends_in`] some constructor family), in order.
fn leaves<'a>(e: &'a Typed, out: &mut Vec<&'a Typed>) {
    match &e.kind {
        TExprKind::Let(_, _, body)
        | TExprKind::LetMut(_, _, body)
        | TExprKind::Assign(_, _, body)
        | TExprKind::Seq(_, body) => leaves(body, out),
        TExprKind::If(_, t, f) => {
            leaves(t, out);
            leaves(f, out);
        }
        TExprKind::Match(_, arms) => {
            for a in arms {
                leaves(&a.body, out);
            }
        }
        _ => out.push(e),
    }
}

/// Rebuild the value-carrying structure of `e` — which [`ends_in`] the
/// constructors `at_leaf` handles — with `at_leaf` applied to each. `like`
/// lends its span and type to every rebuilt node: it is the eliminator being
/// pushed, whose position is what a diagnostic should still point at, and whose
/// type — the payload's, not the constructed value's — is what the rebuilt
/// nodes now have.
fn push(e: &Typed, like: &Typed, at_leaf: &mut dyn FnMut(&Typed, Known<'_>) -> Typed) -> Typed {
    let ty = like.ty.clone();
    let kind = match &e.kind {
        TExprKind::Let(n, v, body) => {
            TExprKind::Let(n.clone(), v.clone(), Box::new(push(body, like, at_leaf)))
        }
        TExprKind::LetMut(n, v, body) => {
            TExprKind::LetMut(n.clone(), v.clone(), Box::new(push(body, like, at_leaf)))
        }
        TExprKind::Assign(lhs, v, body) => {
            TExprKind::Assign(lhs.clone(), v.clone(), Box::new(push(body, like, at_leaf)))
        }
        TExprKind::Seq(a, body) => TExprKind::Seq(a.clone(), Box::new(push(body, like, at_leaf))),
        TExprKind::If(c, t, f) => TExprKind::If(
            c.clone(),
            Box::new(push(t, like, at_leaf)),
            Box::new(push(f, like, at_leaf)),
        ),
        TExprKind::Match(scrutinee, arms) => TExprKind::Match(
            scrutinee.clone(),
            arms.iter()
                .map(|a| Arm {
                    body: push(&a.body, like, at_leaf),
                    ..a.clone()
                })
                .collect(),
        ),
        _ => {
            let leaf = known(e).expect("`push` is only called where `ends_in` holds");
            return at_leaf(e, leaf);
        }
    };
    let mut out = node(kind, ty, like);
    out.annotation = e.annotation.clone();
    out
}

/// `body`, preceded by the evaluation of each of `payloads` that does
/// something — one this pass cannot drop (see [`can_defer`]) — in their
/// original order. A payload that only computes a value is left out: the
/// constructor would have owned it, and nothing else ever looked at it.
fn keep_effects(payloads: &[&Typed], body: Typed, like: &Typed, limits: &Limits) -> Typed {
    payloads
        .iter()
        .rev()
        .filter(|p| !can_defer(p, &limits.blocked))
        .fold(body, |rest, p| {
            let ty = rest.ty.clone();
            node(
                TExprKind::Seq(Box::new((*p).clone()), Box::new(rest)),
                ty,
                like,
            )
        })
}

/// `inner?` with the `?` applied at each constructor `inner` ends in: `ok(v)`
/// and `some(v)` become `v` (`ok()` becomes unit), `err(e)` and `none` keep
/// the `?` — the early return, now taken from inside the branch. `try_at` is
/// the original `?` expression.
fn push_try(inner: &Typed, try_at: &Typed) -> Typed {
    push(inner, try_at, &mut |leaf, kind| match kind {
        Known::Ok(Some(v)) | Known::Some(v) => v.clone(),
        Known::Ok(None) => node(TExprKind::Unit, try_at.ty.clone(), try_at),
        Known::Err(_) | Known::None => node(
            TExprKind::Try(Box::new(leaf.clone())),
            try_at.ty.clone(),
            try_at,
        ),
        Known::Variant { .. } | Known::Struct(_) => {
            unreachable!("`?` only ever meets optional and result constructors")
        }
    })
}

/// The arm of `arms` a leaf constructor named `case` selects: the first whose
/// pattern names it, or the first catch-all. Post-mono every pattern is one of
/// these three, and the `match` is exhaustive, so a leaf always finds one.
fn arm_for<'a>(arms: &'a [Arm], case: &str) -> Option<&'a Arm> {
    arms.iter().find(|a| match &a.pattern {
        Pattern::Ctor { name, .. } => name.split('@').next() == Some(case),
        Pattern::Wildcard | Pattern::Bind(_) => true,
        _ => false,
    })
}

/// Whether pushing `arms` into `scrutinee` is both possible and worth it: every
/// leaf selects an arm whose binders line up with its payloads, no payload
/// reads one of those binders, the copies stay within `max_duplicated`, and no
/// binding the scrutinee introduces captures a name an arm reads. A payload
/// with no type of its own (`some(none)`, `ok([])`) is bound to a binder only
/// when the push can annotate it (`annotated` — see [`match_through_binding`]).
fn match_can_move(scrutinee: &Typed, arms: &[Arm], limits: &Limits, annotated: bool) -> bool {
    let mut ends = Vec::new();
    leaves(scrutinee, &mut ends);
    let mut copied = 0usize;
    let mut used: Vec<&Arm> = Vec::new();
    for leaf in ends {
        let kind = known(leaf).expect("a leaf is a known constructor");
        let Some(arm) = arm_for(arms, kind.case_name()) else {
            return false;
        };
        let payloads = kind.payloads();
        if let Pattern::Ctor { bindings, .. } = &arm.pattern {
            if bindings.len() != payloads.len() {
                return false;
            }
            if !annotated
                && bindings
                    .iter()
                    .zip(&payloads)
                    .any(|(b, p)| b != "_" && contains_context_literal(*p))
            {
                return false;
            }
            let mut read = HashSet::new();
            for p in &payloads {
                read_names(*p, &mut read);
            }
            if bindings.iter().any(|b| read.contains(b)) {
                return false;
            }
        }
        if used.iter().any(|u| std::ptr::eq(*u, arm)) {
            copied += body_size(&arm.body);
        } else {
            used.push(arm);
        }
    }
    if copied > limits.max_duplicated {
        return false;
    }
    let mut read = HashSet::new();
    for arm in arms {
        read_names(&arm.body, &mut read);
    }
    let mut bound = HashSet::new();
    bound_names(scrutinee, &mut bound);
    bound.is_disjoint(&read)
}

/// Every name a binding or pattern anywhere in `e` introduces — the names an
/// arm body pushed into `e` would find in scope.
fn bound_names(e: &Typed, out: &mut HashSet<String>) {
    match &e.kind {
        TExprKind::Let(n, _, _) | TExprKind::LetMut(n, _, _) => {
            out.insert(n.clone());
        }
        TExprKind::Match(_, arms) => {
            for a in arms {
                out.extend(a.pattern.bindings());
            }
        }
        _ => {}
    }
    crate::typed_util::each_child(e, &mut |c| bound_names(c, out));
}

/// `let name = value; .. match (name) { arms } ..` with the `match` pushed into
/// `value` and the binding gone — see the module docs. `None` when the shape
/// does not hold: `name` read anywhere but as that one scrutinee, the scrutinee
/// not reachable through the value-carrying wrappers alone, the value not
/// movable past what sits between, or the push itself refused.
fn match_through_binding(
    name: &str,
    annotation: Option<&ConcreteType>,
    value: &Typed,
    body: &Typed,
    limits: &Limits,
) -> Option<Typed> {
    let uses = uses_of(body, name);
    if uses.count != 1 || uses.repeated || !can_defer(value, &limits.blocked) {
        return None;
    }
    // Moving the value to where it is read must not move it past a write to
    // something it reads.
    let mut written = HashSet::new();
    assigned_names(body, &mut written);
    if !written.is_empty() {
        let mut read = HashSet::new();
        read_names(value, &mut read);
        if !read.is_disjoint(&written) {
            return None;
        }
    }
    let annotate = |case: &str| payload_annotation(annotation, case);
    push_into_scrutinee(name, value, body, limits, annotation.is_some(), &annotate)
}

/// `body` with its one `match (name)` — reached through `let`/`mut`/`set`/`;`
/// wrappers only — replaced by the pushed form of `match (value)`. `None` if
/// the read of `name` sits anywhere else.
fn push_into_scrutinee(
    name: &str,
    value: &Typed,
    body: &Typed,
    limits: &Limits,
    annotated: bool,
    annotate: &dyn Fn(&str) -> Option<ConcreteType>,
) -> Option<Typed> {
    let again =
        |inner: &Typed| push_into_scrutinee(name, value, inner, limits, annotated, annotate);
    let kind = match &body.kind {
        TExprKind::Match(scrutinee, arms) if matches!(&scrutinee.kind, TExprKind::Ident(n) if n == name) =>
        {
            if !match_can_move(value, arms, limits, annotated) {
                return None;
            }
            return Some(push_match(value, arms, body, annotate));
        }
        // `let alias = name; ..` — the one read is an alias, itself read once
        // as the scrutinee further down (the shape two inlined calls leave, the
        // inner one's argument binding the outer one's parameter). Follow it
        // and drop the alias; its annotation is the scrutinee's type too.
        TExprKind::Let(alias, v, rest) if matches!(&v.kind, TExprKind::Ident(n) if n == name) => {
            let uses = uses_of(rest, alias);
            if uses.count != 1 || uses.repeated || alias == name || reads(value, alias) {
                return None;
            }
            let alias_ty = declared(body).cloned();
            return push_into_scrutinee(
                alias,
                value,
                rest,
                limits,
                annotated || alias_ty.is_some(),
                &|case| annotate(case).or_else(|| payload_annotation(alias_ty.as_ref(), case)),
            );
        }
        // A binding of the same name below would shadow it; the read this is
        // looking for cannot be under one. A value the pushed `value` reads
        // cannot be re-bound between either, or the move would capture it.
        TExprKind::Let(n, v, rest) | TExprKind::LetMut(n, v, rest)
            if n != name && !reads(value, n) =>
        {
            let inner = Box::new(again(rest)?);
            if matches!(&body.kind, TExprKind::Let(..)) {
                TExprKind::Let(n.clone(), v.clone(), inner)
            } else {
                TExprKind::LetMut(n.clone(), v.clone(), inner)
            }
        }
        TExprKind::Assign(lhs, v, rest) => {
            TExprKind::Assign(lhs.clone(), v.clone(), Box::new(again(rest)?))
        }
        TExprKind::Seq(first, rest) => TExprKind::Seq(first.clone(), Box::new(again(rest)?)),
        _ => return None,
    };
    let mut out = node(kind, body.ty.clone(), body);
    out.annotation = body.annotation.clone();
    Some(out)
}

/// Whether `e` reads the name `n` anywhere.
fn reads(e: &Typed, n: &str) -> bool {
    let mut names = HashSet::new();
    read_names(e, &mut names);
    names.contains(n)
}

/// The type a binder of `case`'s payload is declared with, read off the
/// scrutinee's declared type `annotation`: the optional's inner type for
/// `some`, a result's side for `ok`/`err`, nothing for a variant case.
fn payload_annotation(annotation: Option<&ConcreteType>, case: &str) -> Option<ConcreteType> {
    match (annotation, case) {
        (Some(ConcreteType::Optional(inner)), "some") => Some((**inner).clone()),
        (Some(ConcreteType::Result(ok, _)), "ok") => Some((**ok).clone()),
        (Some(ConcreteType::Result(_, err)), "err") => Some((**err).clone()),
        _ => None,
    }
}

/// `match (scrutinee) { arms }` with the `match` applied at each constructor
/// `scrutinee` ends in: the leaf's arm, with each binder bound to the payload
/// in its slot. A `_` binder binds nothing, and a payload no binder takes is
/// still evaluated for whatever it does. `match_at` is the original `match`
/// expression; `annotate` gives the type a binder of each case's payload is
/// declared with, when the scrutinee's type is known (see
/// [`match_through_binding`]).
fn push_match(
    scrutinee: &Typed,
    arms: &[Arm],
    match_at: &Typed,
    annotate: &dyn Fn(&str) -> Option<ConcreteType>,
) -> Typed {
    push(scrutinee, match_at, &mut |leaf, kind| {
        let arm = arm_for(arms, kind.case_name()).expect("checked by `match_can_move`");
        let payloads = kind.payloads();
        let annotation = annotate(kind.case_name());
        let bind = |name: String, mut value: Typed, body: Typed| {
            // A declared type is the value's own.
            if let Some(t) = &annotation {
                value.ty = t.clone();
            }
            let ty = body.ty.clone();
            let mut out = node(
                TExprKind::Let(name, Box::new(value), Box::new(body)),
                ty,
                match_at,
            );
            out.annotation = annotation.clone();
            out
        };
        let evaluate = |value: Typed, body: Typed| {
            let ty = body.ty.clone();
            node(
                TExprKind::Seq(Box::new(value), Box::new(body)),
                ty,
                match_at,
            )
        };
        match &arm.pattern {
            // `some(v) => v` — the arm hands its one payload straight back, so
            // the payload *is* the value; binding it first would only copy it.
            Pattern::Ctor { bindings, .. }
                if matches!((&bindings[..], &payloads[..]), ([b], [_])
                    if matches!(&arm.body.kind, TExprKind::Ident(v) if v == b)) =>
            {
                payloads[0].clone()
            }
            // Binders are bound first to last, so the body sees them all; the
            // payloads' own evaluation order is preserved the same way. The body
            // is copied to every leaf, so each copy binds names of its own.
            Pattern::Ctor { bindings, .. } => {
                let renames = fresh_binders(bindings);
                let body = rename_scoped(&arm.body, &renames, true);
                bindings
                    .iter()
                    .zip(payloads)
                    .rev()
                    .fold(body, |body, (binder, payload)| match renames.get(binder) {
                        Some(fresh) => bind(fresh.clone(), payload.clone(), body),
                        None => evaluate(payload.clone(), body),
                    })
            }
            // A catch-all binder names the whole constructed value, which is
            // therefore still built.
            Pattern::Bind(name) => {
                let renames = fresh_binders(std::slice::from_ref(name));
                let body = rename_scoped(&arm.body, &renames, true);
                bind(renames[name].clone(), leaf.clone(), body)
            }
            _ => payloads
                .iter()
                .rev()
                .fold(distinct_binders(&arm.body), |body, payload| {
                    evaluate((*payload).clone(), body)
                }),
        }
    })
}

/// A new name for each of `binders` that binds something (`_` binds nothing).
fn fresh_binders(binders: &[String]) -> std::collections::HashMap<String, String> {
    binders
        .iter()
        .filter(|b| *b != "_")
        .map(|b| (b.clone(), aipl_syntax::fresh_local("ceq", b)))
        .collect()
}

/// Whether projecting `field` out of every struct `base` ends in is possible:
/// each leaf has the field, and every other field can either be dropped (see
/// [`can_defer`]) or kept as a statement. The only thing refused is a field
/// that leaves the function from where it sits, which no statement placed
/// ahead of the projected value could reproduce.
fn field_can_move(base: &Typed, field: &str, limits: &Limits) -> bool {
    let mut ends = Vec::new();
    leaves(base, &mut ends);
    ends.iter().all(|leaf| {
        let Some(Known::Struct(fields)) = known(leaf) else {
            return false;
        };
        fields.iter().any(|f| f.name == field)
            && fields
                .iter()
                .filter(|f| f.name != field)
                .all(|f| can_defer(&f.value, &limits.blocked) || keepable(&f.value))
    })
}

/// Whether an expression can stand as a statement ahead of another value:
/// anything that does not transfer control out of the function, or write a
/// binding, from where it sits. (`can_defer` refuses those too, plus anything
/// effectful; here the effectful ones are exactly what a kept statement is
/// for.)
fn keepable(e: &Typed) -> bool {
    !matches!(
        &e.kind,
        TExprKind::Try(_) | TExprKind::Return(_) | TExprKind::Assign(..)
    ) && !any_child(e, |c| !keepable(c))
}

/// `base.field` with the projection applied at each struct `base` ends in:
/// the field's initializer, behind whichever other fields must still run.
fn push_field(base: &Typed, field: &str, field_at: &Typed, limits: &Limits) -> Typed {
    push(base, field_at, &mut |_, kind| {
        let Known::Struct(fields) = kind else {
            unreachable!("a field is only ever projected out of a struct")
        };
        let projected = fields
            .iter()
            .find(|f| f.name == field)
            .expect("checked by `field_can_move`");
        let others: Vec<&Typed> = fields
            .iter()
            .filter(|f| f.name != field)
            .map(|f| &f.value)
            .collect();
        keep_effects(&others, projected.value.clone(), field_at, limits)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_duplicated: usize) -> Limits {
        Limits {
            max_duplicated,
            blocked: HashSet::from(["loud".to_string()]),
        }
    }

    fn e(kind: TExprKind<ConcreteType>) -> Typed {
        TExpr::new(kind, ConcreteType::Unit, 0..0)
    }

    fn id(n: &str) -> Typed {
        e(TExprKind::Ident(n.into()))
    }

    fn call(c: Callee, args: Vec<Typed>) -> Typed {
        e(TExprKind::Call {
            callee: c,
            type_args: Vec::new(),
            args,
            method_style: false,
        })
    }

    fn user(name: &str, args: Vec<Typed>) -> Typed {
        call(Callee::User(name.into()), args)
    }

    fn arm(name: &str, bindings: &[&str], body: Typed) -> Arm {
        Arm {
            pattern: Pattern::Ctor {
                name: name.into(),
                bindings: bindings.iter().map(|b| (*b).to_string()).collect(),
                ignore_payload: false,
            },
            body,
            span: 0..0,
        }
    }

    fn branch(then: Typed, otherwise: Typed) -> Typed {
        e(TExprKind::If(
            Box::new(id("c")),
            Box::new(then),
            Box::new(otherwise),
        ))
    }

    fn construct(fields: &[(&str, Typed)]) -> Typed {
        e(TExprKind::Construct(
            "S".into(),
            fields
                .iter()
                .map(|(n, v)| TFieldInit {
                    name: aipl_syntax::SpanStr::synthetic((*n).to_string()),
                    value: v.clone(),
                })
                .collect(),
        ))
    }

    #[test]
    fn try_on_result_arms_unwraps_ok_and_keeps_err() {
        let inlined = e(TExprKind::Match(
            Box::new(id("o")),
            vec![
                arm("some", &["v"], call(Callee::Ok, vec![id("v")])),
                arm("none", &[], call(Callee::Err, vec![id("e")])),
            ],
        ));
        let out = rewrite(&e(TExprKind::Try(Box::new(inlined))), &limits(8));
        let TExprKind::Match(_, arms) = &out.kind else {
            panic!("the `?` should have been pushed into the match, got {out:?}");
        };
        assert!(matches!(&arms[0].body.kind, TExprKind::Ident(v) if v == "v"));
        assert!(matches!(
            &arms[1].body.kind,
            TExprKind::Try(inner) if matches!(&inner.kind, TExprKind::Call { callee: Callee::Err, args: _, .. })
        ));
    }

    #[test]
    fn try_on_optional_branches_unwraps_some_and_keeps_none() {
        let optional = branch(call(Callee::Some, vec![id("x")]), e(TExprKind::None));
        let out = rewrite(&e(TExprKind::Try(Box::new(optional))), &limits(8));
        let TExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the `?` inside the `if`, got {out:?}");
        };
        assert!(matches!(&t.kind, TExprKind::Ident(v) if v == "x"));
        assert!(matches!(&f.kind, TExprKind::Try(inner) if matches!(inner.kind, TExprKind::None)));
    }

    #[test]
    fn a_branch_that_is_not_a_constructor_keeps_the_eliminator_outside() {
        let mixed = e(TExprKind::Match(
            Box::new(id("o")),
            vec![
                arm("some", &["v"], call(Callee::Ok, vec![id("v")])),
                arm("none", &[], user("fallback", vec![])),
            ],
        ));
        let out = rewrite(&e(TExprKind::Try(Box::new(mixed))), &limits(8));
        assert!(matches!(out.kind, TExprKind::Try(_)));
    }

    #[test]
    fn the_push_runs_through_bindings_ahead_of_the_tail() {
        let inner = e(TExprKind::Let(
            "e".into(),
            Box::new(id("msg")),
            Box::new(call(Callee::Ok, vec![])),
        ));
        let out = rewrite(&e(TExprKind::Try(Box::new(inner))), &limits(8));
        let TExprKind::Let(_, _, body) = &out.kind else {
            panic!("expected the let to survive, got {out:?}");
        };
        assert!(matches!(body.kind, TExprKind::Unit));
    }

    #[test]
    fn match_on_optional_branches_binds_the_payload_per_leaf() {
        // `value_or` inlined over `if (c) { some(x) } else { none }`.
        let scrutinee = branch(call(Callee::Some, vec![id("x")]), e(TExprKind::None));
        let matched = e(TExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], id("v")), arm("none", &[], id("d"))],
        ));
        let out = rewrite(&matched, &limits(8));
        let TExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the match pushed into the `if`, got {out:?}");
        };
        // `some(v) => v` hands the payload straight back: no binding at all.
        assert!(matches!(&t.kind, TExprKind::Ident(x) if x == "x"));
        assert!(matches!(&f.kind, TExprKind::Ident(d) if d == "d"));
    }

    #[test]
    fn a_match_on_a_let_bound_name_is_pushed_into_the_binding() {
        // `let s: i64? = if c { some(x) } else { none }; let d = 0; match (s)
        // { some(v) => v + 1, none => d }` — inlining's shape.
        let value = branch(call(Callee::Some, vec![id("x")]), e(TExprKind::None));
        let matched = e(TExprKind::Match(
            Box::new(id("s")),
            vec![
                arm("some", &["v"], user("inc", vec![id("v")])),
                arm("none", &[], id("d")),
            ],
        ));
        // The binding says what its value is: an `i64?`.
        let mut value = value;
        value.ty = ConcreteType::Optional(Box::new(ConcreteType::Primitive(
            aipl_syntax::ast::Primitive::I64,
        )));
        let mut bound = e(TExprKind::Let(
            "s".into(),
            Box::new(value),
            Box::new(e(TExprKind::Let(
                "d".into(),
                Box::new(e(TExprKind::Int(0))),
                Box::new(matched),
            ))),
        ));
        bound.annotation = Some(ConcreteType::Optional(Box::new(ConcreteType::Primitive(
            aipl_syntax::ast::Primitive::I64,
        ))));
        let out = rewrite(&bound, &limits(8));
        let TExprKind::Let(d, _, rest) = &out.kind else {
            panic!("expected the `d` binding to survive, got {out:?}");
        };
        assert_eq!(d, "d");
        let TExprKind::If(_, t, f) = &rest.kind else {
            panic!("expected the match pushed into the `if`, got {rest:?}");
        };
        // The binder takes the annotation's payload type.
        assert!(matches!(&t.kind, TExprKind::Let(v, p, _)
            if v.ends_with("_v") && matches!(&p.kind, TExprKind::Ident(x) if x == "x")
                && p.ty == ConcreteType::Primitive(aipl_syntax::ast::Primitive::I64)));
        assert!(t.annotation.is_some());
        assert!(matches!(&f.kind, TExprKind::Ident(d) if d == "d"));
    }

    #[test]
    fn a_match_whose_copies_would_be_too_large_stays() {
        let scrutinee = branch(
            call(Callee::Some, vec![id("x")]),
            branch(call(Callee::Some, vec![id("y")]), e(TExprKind::None)),
        );
        let big = user("f", vec![user("g", vec![id("v")])]);
        let matched = e(TExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], big), arm("none", &[], id("d"))],
        ));
        // Two `some` leaves select the same two-operation arm: one copy of it.
        assert!(matches!(
            rewrite(&matched, &limits(1)).kind,
            TExprKind::Match(..)
        ));
        assert!(matches!(
            rewrite(&matched, &limits(2)).kind,
            TExprKind::If(..)
        ));
    }

    #[test]
    fn a_scrutinee_binding_an_arm_reads_blocks_the_move() {
        let scrutinee = e(TExprKind::Let(
            "d".into(),
            Box::new(id("inner")),
            Box::new(branch(
                call(Callee::Some, vec![id("d")]),
                e(TExprKind::None),
            )),
        ));
        let matched = e(TExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], id("v")), arm("none", &[], id("d"))],
        ));
        assert!(matches!(
            rewrite(&matched, &limits(8)).kind,
            TExprKind::Match(..)
        ));
    }

    #[test]
    fn match_on_variant_cases_binds_each_payload_slot() {
        let scrutinee = branch(user("Pair@V", vec![id("a"), id("b")]), id("Nil@V"));
        let matched = e(TExprKind::Match(
            Box::new(scrutinee),
            vec![
                arm("Pair@V", &["x", "_"], id("x")),
                arm("Nil@V", &[], id("d")),
            ],
        ));
        let out = rewrite(&matched, &limits(8));
        let TExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the match pushed into the `if`, got {out:?}");
        };
        // `let x = a; b; x` — the ignored slot is still evaluated.
        let TExprKind::Let(name, payload, body) = &t.kind else {
            panic!("expected `let x = a; ..`, got {t:?}");
        };
        // The arm's binder is renamed, since its body is copied to each leaf.
        assert!(name.ends_with("_x"));
        assert!(matches!(&payload.kind, TExprKind::Ident(a) if a == "a"));
        assert!(matches!(&body.kind, TExprKind::Seq(first, rest)
            if matches!(&first.kind, TExprKind::Ident(b) if b == "b")
                && matches!(&rest.kind, TExprKind::Ident(x) if x == name)));
        assert!(matches!(&f.kind, TExprKind::Ident(d) if d == "d"));
    }

    #[test]
    fn a_payload_reading_an_arm_binder_blocks_the_move() {
        // `Pair(x, x)` against `Pair(x, y)`: binding `x` first would make the
        // second payload read the binder instead of the outer `x`.
        let scrutinee = user("Pair@V", vec![id("x"), id("x")]);
        let matched = e(TExprKind::Match(
            Box::new(scrutinee),
            vec![arm("Pair@V", &["x", "y"], id("y"))],
        ));
        assert!(matches!(
            rewrite(&matched, &limits(8)).kind,
            TExprKind::Match(..)
        ));
    }

    #[test]
    fn equality_with_a_constructor_asks_the_tag() {
        // `x == some(y)` → `match (x) { some(v) => v == y, none => false }`.
        let compared = call(
            Callee::Equal,
            vec![id("x"), call(Callee::Some, vec![id("y")])],
        );
        let out = rewrite(&compared, &limits(8));
        let TExprKind::Match(scrutinee, arms) = &out.kind else {
            panic!("expected a match on `x`, got {out:?}");
        };
        assert!(matches!(&scrutinee.kind, TExprKind::Ident(x) if x == "x"));
        assert!(
            matches!(&arms[0].pattern, Pattern::Ctor { name, bindings, .. }
            if name == "some" && bindings.len() == 1)
        );
        assert!(
            matches!(&arms[0].body.kind, TExprKind::Call { callee: Callee::Equal, args: a, .. }
            if matches!(&a[1].kind, TExprKind::Ident(y) if y == "y"))
        );
        assert!(
            matches!(&arms[1].pattern, Pattern::Ctor { name, bindings, .. }
            if name == "none" && bindings.is_empty())
        );
        assert!(matches!(arms[1].body.kind, TExprKind::Bool(false)));
    }

    #[test]
    fn inequality_with_an_effectful_payload_binds_it_ahead_and_inverts() {
        // `err(loud()) != r` → `let t = loud(); match (r) { err(v) => v != t, ok => true }`:
        // `loud` prints, so it runs whether or not `r` is an `err`.
        let compared = call(
            Callee::NotEqual,
            vec![call(Callee::Err, vec![user("loud", vec![])]), id("r")],
        );
        let out = rewrite(&compared, &limits(8));
        let TExprKind::Let(bound, value, body) = &out.kind else {
            panic!("expected the payload bound ahead of the match, got {out:?}");
        };
        assert!(
            matches!(&value.kind, TExprKind::Call { callee: Callee::User(f), args: _, .. } if f == "loud")
        );
        let TExprKind::Match(_, arms) = &body.kind else {
            panic!("expected a match under the binding, got {body:?}");
        };
        assert!(
            matches!(&arms[0].body.kind, TExprKind::Call { callee: Callee::NotEqual, args: a, .. }
            if matches!(&a[1].kind, TExprKind::Ident(t) if t == bound))
        );
        assert!(matches!(&arms[1].pattern, Pattern::Ctor { name, .. } if name == "ok"));
        assert!(matches!(arms[1].body.kind, TExprKind::Bool(true)));
    }

    #[test]
    fn equality_with_a_variant_case_compares_every_slot_and_catches_the_rest() {
        let compared = call(
            Callee::Equal,
            vec![id("x"), user("Pair@V", vec![id("a"), id("b")])],
        );
        let out = rewrite(&compared, &limits(8));
        let TExprKind::Match(_, arms) = &out.kind else {
            panic!("expected a match on `x`, got {out:?}");
        };
        assert!(matches!(
            &arms[0].body.kind,
            TExprKind::Call {
                callee: Callee::LogicalAnd,
                args: _,
                ..
            }
        ));
        assert!(matches!(arms[1].pattern, Pattern::Wildcard));
        // A nullary case needs no payload comparison at all.
        let nullary = call(Callee::Equal, vec![id("Nil@V"), id("x")]);
        let TExprKind::Match(_, arms) = &rewrite(&nullary, &limits(8)).kind else {
            panic!("expected a match");
        };
        assert!(matches!(arms[0].body.kind, TExprKind::Bool(true)));
    }

    #[test]
    fn a_pure_payload_moves_into_the_arm_and_a_nested_constructor_unwraps_again() {
        // `a == Cons(1, Cons(2, Nil))`: the inner `Cons` is pure, so it goes into
        // the arm as the operand of `v1 == Cons(2, Nil)` — itself unwrapped.
        let inner = user("Cons@L", vec![id("two"), id("Nil@L")]);
        let compared = call(
            Callee::Equal,
            vec![id("a"), user("Cons@L", vec![id("one"), inner])],
        );
        let out = rewrite(&compared, &limits(8));
        let TExprKind::Match(_, arms) = &out.kind else {
            panic!("expected a match on `a` with no binding ahead, got {out:?}");
        };
        let TExprKind::Call {
            callee: Callee::LogicalAnd,
            args: parts,
            ..
        } = &arms[0].body.kind
        else {
            panic!("expected `v0 == one && ..`, got {:?}", arms[0].body);
        };
        assert!(matches!(parts[1].kind, TExprKind::Match(..)));
    }

    #[test]
    fn a_comparison_of_two_constructors_is_left_alone() {
        let compared = call(
            Callee::Equal,
            vec![
                call(Callee::Some, vec![id("a")]),
                call(Callee::Some, vec![id("b")]),
            ],
        );
        assert!(matches!(
            rewrite(&compared, &limits(8)).kind,
            TExprKind::Call {
                callee: Callee::Equal,
                ..
            }
        ));
    }

    #[test]
    fn an_unwrapped_comparison_then_takes_the_match_rule() {
        // `(if c { some(a) } else { none }) == some(y)` → `if c { a == y } else { false }`.
        let compared = call(
            Callee::Equal,
            vec![
                branch(call(Callee::Some, vec![id("a")]), e(TExprKind::None)),
                call(Callee::Some, vec![id("y")]),
            ],
        );
        let out = rewrite(&compared, &limits(8));
        let TExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the match pushed into the `if`, got {out:?}");
        };
        assert!(matches!(&t.kind, TExprKind::Let(_, _, body)
            if matches!(body.kind, TExprKind::Call { callee: Callee::Equal, .. })));
        assert!(matches!(f.kind, TExprKind::Bool(false)));
    }

    #[test]
    fn field_of_a_construction_is_its_initializer() {
        let projected = e(TExprKind::Field(
            Box::new(branch(
                construct(&[("a", id("x")), ("b", user("pure", vec![]))]),
                construct(&[("a", id("y")), ("b", id("z"))]),
            )),
            "a".into(),
        ));
        let out = rewrite(&projected, &limits(8));
        let TExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the projection pushed into the `if`, got {out:?}");
        };
        assert!(matches!(&t.kind, TExprKind::Ident(x) if x == "x"));
        assert!(matches!(&f.kind, TExprKind::Ident(y) if y == "y"));
    }

    #[test]
    fn a_dropped_field_that_does_something_is_kept_as_a_statement() {
        let projected = e(TExprKind::Field(
            Box::new(construct(&[("a", id("x")), ("b", user("loud", vec![]))])),
            "a".into(),
        ));
        let out = rewrite(&projected, &limits(8));
        assert!(matches!(&out.kind, TExprKind::Seq(first, rest)
            if matches!(&first.kind, TExprKind::Call { callee: Callee::User(n), args: _, .. } if n == "loud")
                && matches!(&rest.kind, TExprKind::Ident(x) if x == "x")));
    }

    #[test]
    fn a_field_that_leaves_the_function_blocks_the_move() {
        let projected = e(TExprKind::Field(
            Box::new(construct(&[
                ("a", id("x")),
                ("b", e(TExprKind::Try(Box::new(id("r"))))),
            ])),
            "a".into(),
        ));
        assert!(matches!(
            rewrite(&projected, &limits(8)).kind,
            TExprKind::Field(..)
        ));
    }
}
