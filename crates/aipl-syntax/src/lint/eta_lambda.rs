use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::{Error, Span};
use std::collections::HashSet;

use super::{mentions, quote};

/// What the lint needs to know about the file: which name its `map` goes by, and
/// which names `$` can call.
pub(super) struct Forwarding {
    map: Option<String>,
    /// The names that follow a `$` as `xs$f()`: a function of this file whose
    /// first parameter is named `self`, which is what method-call syntax is
    /// defined by, and a lowercase builtin the file imports. A function from
    /// another file is not here — its parameters are not in view when a file is
    /// linted — so it is never advised.
    methods: HashSet<String>,
}

impl Forwarding {
    pub(super) fn of(program: &Program, map: Option<String>) -> Self {
        let mut methods = HashSet::new();
        for item in &program.items {
            match item {
                Item::Fn(f) if f.sig.params.first().is_some_and(|p| p.name == "self") => {
                    methods.insert(f.name.text.clone());
                }
                Item::Import(decl) if matches!(decl.source, ImportSource::Builtins { .. }) => {
                    methods.extend(
                        decl.names
                            .iter()
                            .map(|n| n.local())
                            .filter(|n| n.starts_with(|c: char| c.is_ascii_lowercase() || c == '_'))
                            .map(str::to_string),
                    );
                }
                _ => {}
            }
        }
        Forwarding { map, methods }
    }
}

/// A lambda that adds nothing to what it forwards its argument to.
///
/// **In a `map`**, a lambda whose whole body is its parameter taken through a
/// chain of member reads and method calls — `|x| x.name`, `|x| x.0`,
/// `|x| x.f(args)`, `|x| x.a.f(b).c`, `|x| to_str(x)` — is `xs$name`, `xs$0`,
/// `xs$f(args)`, `xs$a$f(b)$c` and `xs$to_str()`: each step written once, with no
/// name for the element to introduce and then repeat. And a bare function name,
/// `xs.map(f)`, is `xs$f()`.
///
/// Only the shape `$` stands for qualifies. The parameter has to be the root of
/// the chain and appear nowhere else (`|x| x.f(x)` needs the name), it has to be
/// unannotated, since `$` has no place to write a type, and every call in the
/// chain has to be one `$` can make — see [`Forwarding::methods`], or already be
/// written as a method call. An operator is never a step: `|x| x + 1` does
/// something with `x`.
///
/// **Anywhere else**, a lambda that only forwards its arguments to one call,
/// `|x| to_str(x)`, is the function itself: pass `to_str`.
///
/// The hit points at the lambda, or at the bare name — a short span that begins on
/// the line the `#[allow]` would be written on. The lint runs before checking, so
/// it cannot tell a sequence from an optional; `$` maps either, as `map` does.
///
/// `taken` is the spans of lambdas already advised on, so that the visit of the
/// lambda itself, which follows its `map` call, adds nothing.
pub(super) fn eta_lambda(
    e: &Expr,
    src: &str,
    cx: &Forwarding,
    taken: &mut Vec<Span>,
    hits: &mut Vec<Error>,
) {
    match &e.kind {
        ExprKind::Call(callee, args, _) if cx.map.as_deref().is_some_and(|m| callee == m) => {
            if let [recv, arg] = args.as_slice() {
                map_step(recv, arg, src, cx, taken, hits);
            }
        }
        ExprKind::Lambda(params, body) if !taken.contains(&e.span) => {
            forwarded(e, params, body, hits);
        }
        _ => {}
    }
}

/// The advice for `recv.map(arg)`, if `arg` is a function name or a lambda `$` can
/// say.
fn map_step(
    recv: &Expr,
    arg: &Expr,
    src: &str,
    cx: &Forwarding,
    taken: &mut Vec<Span>,
    hits: &mut Vec<Error>,
) {
    let recv = quote(recv, src, "<items>");
    match &arg.kind {
        ExprKind::Ident(name) if cx.methods.contains(name) => {
            hits.push(Error::at(
                format!("\"map\" over the function \"{name}\" — write \"{recv}${name}()\""),
                arg.span.clone(),
            ));
        }
        ExprKind::Lambda(params, body) => {
            let [param] = params.as_slice() else {
                return;
            };
            if param.ty.is_some() || param.name.contains('$') {
                return;
            }
            let Some(steps) = chain(body, &param.name, cx) else {
                return;
            };
            if steps.is_empty() {
                return;
            }
            taken.push(arg.span.clone());
            hits.push(Error::at(
                format!(
                    "\"map\" with a lambda that only uses a member of its argument — write \
                     \"{recv}{}\"",
                    steps.concat()
                ),
                arg.span.clone(),
            ));
        }
        _ => {}
    }
}

/// The `$` steps `body` is made of, outermost last, when `body` is `param`
/// taken through member reads and calls and nothing else; `None` otherwise.
fn chain(body: &Expr, param: &str, cx: &Forwarding) -> Option<Vec<String>> {
    match &body.kind {
        ExprKind::Ident(name) if name == param => Some(Vec::new()),
        ExprKind::Field(inner, name) => {
            let mut steps = chain(inner, param, cx)?;
            // A tuple slot is the field `_0`, written `0`.
            let shown = match name.strip_prefix('_') {
                Some(slot) if !slot.is_empty() && slot.bytes().all(|b| b.is_ascii_digit()) => slot,
                _ => name,
            };
            steps.push(format!("${shown}"));
            Some(steps)
        }
        ExprKind::Call(name, args, method_style) => {
            let (first, rest) = args.split_first()?;
            // An operator use is a call named for its spelling.
            if !crate::operator_named_forms(name.name()).is_empty()
                || name == param
                || rest.iter().any(|a| mentions(a, param))
                || !(*method_style || cx.methods.contains(name.name()))
            {
                return None;
            }
            let mut steps = chain(first, param, cx)?;
            let shown = if rest.is_empty() { "" } else { "..." };
            steps.push(format!("${name}({shown})"));
            Some(steps)
        }
        _ => None,
    }
}

/// A lambda that only forwards its argument(s) to one call, outside a `map`
/// that [`map_step`] handles.
fn forwarded(e: &Expr, params: &[crate::ast::LambdaParam], body: &Expr, hits: &mut Vec<Error>) {
    // A lambda the parser wrote for `xs$f()` names its parameter with a `$`, which
    // no source can; there is no longer spelling for it to be advised into.
    if params.iter().any(|p| p.name.contains('$')) {
        return;
    }
    let ExprKind::Call(name, args, _) = &body.kind else {
        return;
    };

    // An operator use is a call named for its spelling, so `|a, b| a + b` has
    // the shape this lint looks for. It is left alone deliberately: the language
    // does accept `+` as a value (see `cases/lambdas/operator_as_value`), but
    // `xs.fold(0, +)` is a different reading from `|acc, x| acc + x` rather than
    // an obviously better one, and this lint's job is the case where the lambda
    // adds *nothing* — `|x| to_str(x)` against `to_str`. Deciding the operator
    // question belongs to whoever wants to decide it, corpus-wide, on purpose.
    if !crate::operator_named_forms(name.name()).is_empty() {
        return;
    }
    if args.len() != params.len() || params.iter().any(|p| p.name == *name) {
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
             \"{name}\" directly"
        ),
        e.span.clone(),
    ));
}
