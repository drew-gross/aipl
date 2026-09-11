use crate::ast::{ExprKind, ImportSource, Item, Pattern, Program, Type};
use crate::{each_expr, Error};
use std::collections::HashSet;

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
pub(super) fn unused_imports(program: &Program, hits: &mut Vec<Error>) {
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
                    for slot in &case.payload {
                        collect_ty_names(&slot.ty, &mut out);
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
        // No arm for operators: an operator use is a `Call` whose callee is the
        // spelling, so the arm above already records `+`, `==`, `!` and the rest
        // exactly as it records any other name. Unary `-` is the one operator
        // with no named builtin behind it, so it stays a node and needs its own.
        ExprKind::Neg(_) => {
            out.insert("-".to_string());
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
                collect_pattern_names(&arm.pattern, &mut out);
            }
        }
        // `if (let P = e)` is one arm without the `match`; its pattern names
        // a constructor just as an arm's does.
        ExprKind::IfLet(arm, _, _) => collect_pattern_names(&arm.pattern, &mut out),
        _ => {}
    });
    out
}

/// The constructor a pattern names, if it names one. A `V.A` path names the
/// variant as well as the case.
fn collect_pattern_names(pattern: &Pattern, out: &mut HashSet<String>) {
    if let Pattern::Ctor { name, .. } = pattern {
        if let Some((v, _)) = name.split_once('.') {
            out.insert(v.to_string());
        }
        out.insert(name.clone());
    }
}

/// Every name `ty` mentions, so an import used only in a type position still
/// counts as used. Variants are spelled out rather than wildcarded so a new
/// `Type` forces a decision here.
fn collect_ty_names(ty: &Type, out: &mut HashSet<String>) {
    match ty {
        Type::Named(n) => {
            out.insert(n.clone());
        }
        // `Case<V>` mentions `V`, so importing a variant to name one of its
        // cases counts as using the import.
        Type::Case(v) => collect_ty_names(v, out),
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
