//! Lints: *squelchable* errors. AIPL has no warnings — every diagnostic is an
//! error and fails the compile — but the errors this module produces (and only
//! these) can be squelched by appending `#[allow]` to the offending line. The
//! marker is line-scoped: it silences every lint whose reported span starts on
//! its line, and nothing else. Regular errors (type mismatches, unknown names,
//! parse errors, ...) take no notice of `#[allow]`.
//!
//! A lint flags code that is *legal but has a clearly better spelling*; its
//! message must name that better spelling. The loader runs `aipl_mono::check` on every
//! file right after parsing (the markers come from the lexer via
//! `parse_with_allows`), so lints fire before type checking.

mod compound_assign;
mod destructure_binding;
mod destructure_param;
mod eta_lambda;
mod field_init_shorthand;
mod fn_body_type_stutter;
mod is_empty_longhand;
mod len_gt_zero;
mod match_is_some_and;
mod match_map_err;
mod match_map_ok;
mod match_value_or;
mod match_value_or_err;
mod push_array_literal;
mod push_loop_pipeline;
mod return_loop_find_if;
mod return_loop_find_index;
mod slice_from_zero;
mod slice_to_len;
mod slice_whole;
mod step_by_one;
mod unused_imports;

use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::{each_expr, each_subexpr, Error, Span};
use std::collections::{HashMap, HashSet};

use self::compound_assign::{compound_assign, matching_compound};
use self::destructure_binding::destructure_binding;
use self::destructure_param::destructure_param;
use self::eta_lambda::eta_lambda;
use self::field_init_shorthand::field_init_shorthand;
use self::fn_body_type_stutter::fn_body_type_stutter;
use self::is_empty_longhand::{empty_names, is_empty_longhand};
use self::len_gt_zero::{len_gt_zero, len_zero_cmp};
use self::match_is_some_and::match_is_some_and;
use self::match_map_err::match_map_err;
use self::match_map_ok::match_map_ok;
use self::match_value_or::match_value_or;
use self::match_value_or_err::match_value_or_err;
use self::push_array_literal::push_array_literal;
use self::push_loop_pipeline::{pipeline_names, push_loop_pipeline};
use self::return_loop_find_if::{find_if_name, return_loop_find_if};
use self::return_loop_find_index::{find_index_name, return_loop_find_index};
use self::slice_from_zero::slice_from_zero;
use self::slice_to_len::slice_to_len;
use self::slice_whole::slice_whole;
use self::step_by_one::{matching_steps, step_by_one};
use self::unused_imports::unused_imports;

/// Run every lint over `program` — function bodies, `.test` blocks, and
/// keyword-parameter / struct-field default expressions — then drop the
/// hits squelched by a same-line `#[allow]` (`allows` are the marker spans
/// the lexer collected). Returns every surviving lint error.
pub fn check(program: &Program, src: &str, allows: &[Span]) -> Result<(), Vec<Error>> {
    let allowed: HashSet<usize> = allows.iter().map(|sp| line_of(src, sp.start)).collect();
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
    // Not `each_expr`: this lint needs to know whether the *enclosing* function
    // declares any effects. AIPL requires a caller to declare at least the
    // effects of everything it calls, so "declares none" is a sound local proof
    // that the `none` arm's error is effect-free — which is what decides whether
    // the optimizer may sink it. A `.test` body runs with every effect allowed,
    // so it counts as effectful. Expressions outside a function body (a struct
    // field or keyword-parameter default) have no enclosing signature to ask and
    // are simply not visited.
    for item in &program.items {
        if let Item::Fn(func) = item {
            let pure = func.sig.effects.is_empty();
            crate::each_subexpr(&func.body, &mut |e| {
                match_value_or_err(e, src, pure, &mut hits)
            });
            if let Some(tb) = &func.test_body {
                crate::each_subexpr(tb, &mut |e| match_value_or_err(e, src, false, &mut hits));
            }
        }
    }
    each_expr(program, &mut |e| match_map_err(e, &mut hits));
    each_expr(program, &mut |e| match_map_ok(e, &mut hits));
    each_expr(program, &mut |e| field_init_shorthand(e, src, &mut hits));
    // Only where this file's `push` is the builtin — see `pipeline_names`,
    // which also reports what `map`/`filter` are called here.
    let pipeline = pipeline_names(program);
    each_expr(program, &mut |e| {
        push_loop_pipeline(e, src, &pipeline, &mut hits)
    });
    // Same precondition, and the two shapes are disjoint: a seed followed by
    // a loop is the pipeline's, a seed followed by pushes is the literal's.
    if let Some(push) = imported_as(program, "push") {
        each_expr(program, &mut |e| {
            push_array_literal(e, src, &push, &mut hits)
        });
    }
    // The shape is all language syntax, so this one fires whether or not the
    // file imported `find_if` — the name is only for the advice, which says to
    // import it when it is missing.
    let find_if = find_if_name(program);
    each_expr(program, &mut |e| {
        return_loop_find_if(e, src, find_if.as_deref(), &mut hits)
    });
    // The same loop returning the *position* instead. Disjoint from the one
    // above: a loop returns the element or its index, never both.
    let find_index = find_index_name(program);
    each_expr(program, &mut |e| {
        return_loop_find_index(e, src, find_index.as_deref(), &mut hits)
    });
    // Only where a `++`/`--` flavor provably matches the operator written —
    // see `matching_steps`, which also names the import when it's missing.
    let steps = matching_steps(program);
    if !steps.is_empty() {
        each_expr(program, &mut |e| step_by_one(e, &steps, &mut hits));
    }
    // The general form of the above, and partitioned against it rather than
    // overlapping: `set x = x + 1;` belongs to the step lint whenever that lint
    // is live here, so `compound_assign` is told and leaves that one shape
    // alone. Only the operators whose compound spelling provably means the same
    // thing are advised — see `matching_compound`.
    let compound = matching_compound(program);
    if !compound.is_empty() {
        each_expr(program, &mut |e| {
            compound_assign(e, &compound, &steps, &mut hits)
        });
    }
    // Only for the comparisons this file's `<`/`>` really are — see
    // `len_zero_cmp`, which also says whether `is_nonempty` needs importing.
    let cmp = len_zero_cmp(program);
    each_expr(program, &mut |e| len_gt_zero(e, src, &cmp, &mut hits));
    // The mirror of the above: `<`/`>` against zero go to `is_nonempty`, `== 0`
    // and the negated predicate go to `is_empty`. Same import caution.
    let empties = empty_names(program);
    each_expr(program, &mut |e| {
        is_empty_longhand(e, src, &empties, &mut hits)
    });
    unused_imports(program, &mut hits);
    destructure_binding(program, &mut hits);
    destructure_param(program, &mut hits);
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

/// The local name this file's builtins `len` goes by, or `None` when `len` here
/// is **not** the builtin — either the file never imported it, or it defines a
/// function of that name.
///
/// A user's `len` is a different question with a different answer:
/// `tests/cases/variants/recursive_list.aipl` has `fn len(self: List) -> i64`,
/// and `Nil.len() == 0` there is a list-length test, not an emptiness test on a
/// sequence. `is_empty`/`is_nonempty` do not apply to it — they are refused on a
/// variant — so a lint that advised them would advise code that does not
/// compile. Both length lints ask this before flagging anything.
///
/// The import is the whole test: a file defining its own `len` cannot also
/// import the builtin one (the loader refuses a builtin import that collides
/// with a local item), so "imported from builtins" and "not shadowed" are the
/// same condition here.
fn builtin_len_name(program: &Program) -> Option<String> {
    imported_as(program, "len")
}

/// The local name this file's `import { .. } from builtins;` gives `builtin`,
/// or `None` when it never imported it. Matched on the *exported* name, so an
/// alias (`filter as keep`) is followed rather than missed, and only a builtins
/// import counts — a user function of the same name is not this builtin.
fn imported_as(program: &Program, builtin: &str) -> Option<String> {
    program.items.iter().find_map(|item| {
        let Item::Import(decl) = item else {
            return None;
        };
        if !matches!(decl.source, ImportSource::Builtins { .. }) {
            return None;
        }
        decl.names
            .iter()
            .find(|n| n.name == builtin)
            .map(|n| n.local().to_string())
    })
}

/// Whether `e` can be moved into a lambda unchanged. A `?` propagation returns
/// from the function it is written in, so relocating it into a lambda would
/// return from the *lambda* instead: same source, a different early exit. Every
/// lint whose advice lifts an expression into a lambda has to refuse that.
fn lambda_safe(e: &Expr) -> bool {
    let mut ok = true;
    crate::each_subexpr(e, &mut |x| {
        if matches!(x.kind, ExprKind::Try(_)) {
            ok = false;
        }
    });
    ok
}

/// `set acc.push(elem);` (or its longhand `set acc = acc.push(elem);`) — the
/// pushed element and the statements that follow, when `stmt` is a push onto
/// `acc` and nothing else. `push` is the calling file's local name for the
/// builtin, so an aliased import is followed and a *user* function that
/// happens to be called `push` is not.
///
/// Shared by the two push lints, which have to agree on what a push is: they
/// differ only in what surrounds one.
fn pushed_element<'a>(stmt: &'a Expr, acc: &str, push: &str) -> Option<(&'a Expr, &'a Expr)> {
    let ExprKind::Assign(lhs, value, rest) = &stmt.kind else {
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
    Some((&args[1], rest))
}

/// 0-based line number of byte offset `pos` in `src`.
fn line_of(src: &str, pos: usize) -> usize {
    src[..pos.min(src.len())].matches('\n').count()
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

/// The synthetic value the parser ends a statement-only block with: `()` for an
/// ordinary block, and an i64 `0` — the loop expression's own result — for a
/// loop body, which may not end in a value of its own at all. The loop's zero is
/// told from a written one by its empty span, the same way
/// [`slice_from_zero`](slice_from_zero()) tells an elided slice bound from a
/// typed one.
fn is_block_tail(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Unit => true,
        ExprKind::Num(0) => e.span.is_empty(),
        _ => false,
    }
}

/// The single statement a block consists of, or `None` when it holds anything
/// else — no statements, or more than one.
///
/// A block is a right-nested chain, and which node carries the rest of it
/// depends on the statement: an expression statement is a [`ExprKind::Seq`]
/// whose second operand is the remainder, while `let`/`mut`/`set` each hold
/// their own. Only the two forms the loop lints can be made of are recognized
/// here, and each is "lone" exactly when what follows it is the block's tail
/// (see [`is_block_tail`]).
fn lone_stmt(block: &Expr) -> Option<&Expr> {
    match &block.kind {
        ExprKind::Seq(stmt, rest) if is_block_tail(rest) => Some(stmt),
        ExprKind::Assign(_, _, rest) if is_block_tail(rest) => Some(block),
        _ => None,
    }
}

/// Whether `e`'s span covers exactly the text that spells it, so the source can
/// be spliced back into advice. Only a name and a field path off one qualify:
/// their spans run from the first character to the last. Most other forms do
/// not — a call's span stops before its closing paren, so `f(x)` splices back as
/// `f(x`, which doesn't parse — and the ones that might are left out rather than
/// each having to be re-checked whenever a span moves.
fn spans_its_text(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Ident(_) => true,
        ExprKind::Field(recv, _) => spans_its_text(recv),
        _ => false,
    }
}

// ---------- shared by the two destructuring lints ----------
//
// `destructure_binding` asks whether a `let` can become a pattern and
// `destructure_param` asks the same of a parameter. The question is identical
// once the thing being asked about has a name and a scope, so the answering
// lives here and each lint contributes only what is its own: which names are
// candidates, and what the advice reads like.

/// Every `base.field` read in `e`, counted per `(field, base)` pair.
///
/// Counted rather than collected because [`shared_fields`] subtracts one
/// subtree's reads from another's.
fn field_reads(e: &Expr) -> HashMap<(String, String), usize> {
    let mut out: HashMap<(String, String), usize> = HashMap::new();
    each_subexpr(e, &mut |x| {
        if let ExprKind::Field(base, f) = &x.kind {
            if let ExprKind::Ident(b) = &base.kind {
                *out.entry((f.clone(), b.clone())).or_default() += 1;
            }
        }
    });
    out
}

/// Field names read off more than one base, discounting the reads inside
/// `exclude` when there is one — a `let`'s own right-hand side, which runs
/// before the pattern binds anything.
fn shared_fields(
    all: &HashMap<(String, String), usize>,
    exclude: Option<&Expr>,
) -> HashSet<String> {
    let inside = exclude.map(field_reads).unwrap_or_default();
    let mut bases: HashMap<&str, HashSet<&str>> = HashMap::new();
    for ((field, base), n) in all {
        if *n
            > inside
                .get(&(field.clone(), base.clone()))
                .copied()
                .unwrap_or(0)
        {
            bases.entry(field).or_default().insert(base);
        }
    }
    bases
        .into_iter()
        .filter(|(_, b)| b.len() > 1)
        .map(|(f, _)| f.to_string())
        .collect()
}

/// How `name` is used inside `body`.
///
/// `mentions == field_reads` is the whole question both lints turn on: it says
/// the value itself is never used, only its parts, which is exactly when a
/// pattern can replace it.
struct Uses {
    mentions: usize,
    field_reads: usize,
    /// The distinct fields read, in first-read order — the order the advised
    /// pattern lists them in.
    fields: Vec<String>,
    /// Whether something inside `body` rebinds `name`, which splits the
    /// mentions between two values this walk cannot tell apart.
    shadowed: bool,
}

fn uses_of(body: &Expr, name: &str) -> Uses {
    let mut u = Uses {
        mentions: 0,
        field_reads: 0,
        fields: Vec::new(),
        shadowed: false,
    };
    each_subexpr(body, &mut |x| {
        match &x.kind {
            ExprKind::Ident(n) if n == name => u.mentions += 1,
            ExprKind::Field(base, f) if matches!(&base.kind, ExprKind::Ident(n) if n == name) => {
                u.field_reads += 1;
                if !u.fields.iter().any(|k| k == f) {
                    u.fields.push(f.clone());
                }
            }
            _ => {}
        }
        u.shadowed |= rebinds(x, name);
    });
    u
}

/// Whether `e` mentions any of `names` as a bare identifier, ignoring `skip`.
fn mentions_any(e: &Expr, names: &[String], skip: Option<&str>) -> bool {
    let mut found = false;
    each_subexpr(e, &mut |x| {
        if let ExprKind::Ident(n) = &x.kind {
            found |= Some(n.as_str()) != skip && names.iter().any(|f| f == n);
        }
    });
    found
}

/// Whether any of `fields` is a tuple position (`t._0`).
///
/// No struct pattern can take a tuple apart — there is no type name to write —
/// and `let (a, b) = t;` has to name every position, where this sees only the
/// ones that were read. So tuples are left alone rather than advised into a
/// pattern that would not compile.
fn has_tuple_field(fields: &[String]) -> bool {
    fields.iter().any(|f| {
        f.strip_prefix('_')
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Whether `e` introduces a binding called `name`, in any of the positions that
/// can: the two `let` forms, a lambda parameter, a loop variable, and a match
/// arm's payload binders.
fn rebinds(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Let(n, _, _, _) | ExprKind::LetMut(n, _, _, _) | ExprKind::For(n, _, _) => {
            n == name
        }
        ExprKind::Lambda(params, _) => params.iter().any(|p| p.name == name),
        ExprKind::Match(_, arms) => arms
            .iter()
            .any(|a| a.pattern.bindings().iter().any(|b| b == name)),
        _ => false,
    }
}
