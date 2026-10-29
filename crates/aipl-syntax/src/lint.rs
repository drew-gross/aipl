//! Lints: *squelchable* errors. AIPL has no warnings — every diagnostic is an
//! error and fails the compile — but the errors this module produces (and only
//! these) can be squelched by appending `#[allow]` to the offending line. The
//! marker is line-scoped: it silences every lint whose reported span starts on
//! its line, and nothing else. Regular errors (type mismatches, unknown names,
//! parse errors, ...) take no notice of `#[allow]`. A marker on a line where no
//! lint fires is itself an error, so a marker never outlives the code it excused.
//!
//! A lint flags code that is *legal but has a clearly better spelling*; its
//! message must name that better spelling. The loader runs `aipl_mono::check` on every
//! file right after parsing (the markers come from the lexer via
//! `parse_with_allows`), so lints fire before type checking.

mod arm_block_value;
mod char_set_literal;
mod compound_assign;
mod eta_lambda;
mod extend_longhand;
mod field_init_shorthand;
mod interp_literal;
mod interp_to_str;
mod is_empty_longhand;
mod len_gt_zero;
mod let_else_as_case;
mod map_constant_repeat;
mod map_find_if_some;
mod match_case_as_case;
mod match_case_try_as_case;
mod match_is_some_and;
mod match_map_err;
mod match_map_ok;
mod match_map_value_or;
mod match_value_or;
mod match_value_or_err;
mod push_array_literal;
mod push_loop_collect_ok;
mod push_loop_pipeline;
mod return_loop_any_all;
mod return_loop_find_if;
mod return_loop_find_index;
mod set_spread_field;
mod set_spread_push;
mod step_by_one;
mod union_loop_union_all;
mod value_or_compare;
mod with_value_or_self;

use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::{each_expr, each_subexpr, Error, Span};
use std::collections::HashSet;

use self::arm_block_value::arm_block_value;
use self::char_set_literal::char_set_literal;
use self::compound_assign::{compound_assign, matching_compound};
use self::eta_lambda::{eta_lambda, Forwarding};
use self::extend_longhand::{extend_longhand, extend_names};
use self::field_init_shorthand::field_init_shorthand;
use self::interp_literal::interp_literal;
use self::interp_to_str::{interp_to_str, to_str_name};
use self::is_empty_longhand::{empty_names, is_empty_longhand};
use self::len_gt_zero::{len_gt_zero, len_zero_cmp};
use self::let_else_as_case::let_else_as_case;
use self::map_constant_repeat::{map_constant_repeat, repeat_names};
use self::map_find_if_some::{find_map_names, map_find_if_some};
use self::match_case_as_case::match_case_as_case;
use self::match_case_try_as_case::{match_case_try_as_case, tail_matches};
use self::match_is_some_and::match_is_some_and;
use self::match_map_err::match_map_err;
use self::match_map_ok::match_map_ok;
use self::match_map_value_or::match_map_value_or;
use self::match_value_or::match_value_or;
use self::match_value_or_err::match_value_or_err;
use self::push_array_literal::push_array_literal;
use self::push_loop_collect_ok::{collect_ok_names, push_loop_collect_ok};
use self::push_loop_pipeline::{pipeline_names, push_loop_pipeline};
use self::return_loop_any_all::{any_all_names, return_loop_any_all};
use self::return_loop_find_if::{find_if_name, return_loop_find_if};
use self::return_loop_find_index::{find_index_name, return_loop_find_index};
use self::set_spread_field::set_spread_field;
use self::set_spread_push::{append_names, set_spread_push};
use self::step_by_one::{matching_steps, step_by_one};
use self::union_loop_union_all::{union_loop_union_all, union_names};
use self::value_or_compare::{value_or_compare, value_or_compare_names};
use self::with_value_or_self::{with_names, with_value_or_self};

/// What a lint's message ends with: the way to keep what it flagged. Appended by
/// [`check`] to each hit it returns, so a lint's message says only what is wrong
/// and what to write instead.
const SQUELCH_ADVICE: &str = " (or append #[allow] to this line to keep it)";

/// Run every lint over `program` — function bodies, `.test` blocks, and
/// keyword-parameter / struct-field default expressions — then drop the
/// hits squelched by a same-line `#[allow]` (`allows` are the marker spans
/// the lexer collected). Returns every surviving lint error.
pub fn check(
    program: &Program,
    src: &str,
    allows: &[Span],
    ported: Vec<Error>,
) -> Result<(), Vec<Error>> {
    let allowed: HashSet<usize> = allows.iter().map(|sp| line_of(src, sp.start)).collect();
    // The lines whose marker squelched a hit; a marker on any other line is
    // reported below.
    let mut squelched: HashSet<usize> = HashSet::new();
    let mut hits: Vec<Error> = Vec::new();
    // `xs.map(|x| x.name)` is `xs$name`, `xs.map(f)` is `xs$f()`, and a lambda that
    // only forwards its arguments to a call is that function.
    let map = imported_as(program, "map");
    let forwarding = Forwarding::of(program, map.clone());
    let mut advised: Vec<Span> = Vec::new();
    each_expr(program, &mut |e| {
        eta_lambda(e, src, &forwarding, &mut advised, &mut hits)
    });
    // A `to_str` inside a template interpolation, which already renders its
    // value the same way. Only where the file's `to_str` is the builtin.
    if let Some(to_str) = to_str_name(program) {
        each_expr(program, &mut |e| interp_to_str(e, src, &to_str, &mut hits));
    }
    // A string or char literal as an interpolation, which is just its text.
    each_expr(program, &mut |e| interp_literal(e, src, &mut hits));
    // A char set spelled out element by element, which a string says better.
    // Fires whether or not the file imported `to_set` — the name is only for
    // the advice, which says to import it when it is missing.
    let to_set = imported_as(program, "to_set");
    each_expr(program, &mut |e| {
        char_set_literal(e, src, to_set.as_deref(), &mut hits)
    });
    // `map(f).find_if(is_some)` is `find_map(f)` — see `find_map_names` for the
    // three names the shape is made of.
    let find_map = find_map_names(program);
    each_expr(program, &mut |e| map_find_if_some(e, &find_map, &mut hits));
    // `xs.map(|x| c)` with a constant `c` is `[c].repeat(xs.len())`.
    let repeat = repeat_names(program);
    each_expr(program, &mut |e| {
        map_constant_repeat(e, src, &repeat, &mut hits)
    });
    each_expr(program, &mut |e| match_is_some_and(e, &mut hits));
    // A match arm whose block is only its value — braces around nothing.
    each_expr(program, &mut |e| arm_block_value(e, src, &mut hits));
    each_expr(program, &mut |e| match_value_or(e, src, &mut hits));
    each_expr(program, &mut |e| match_map_value_or(e, src, &mut hits));
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
    let as_case = imported_as(program, "as_case");
    each_expr(program, &mut |e| {
        match_case_as_case(e, src, as_case.as_deref(), &mut hits)
    });
    for item in &program.items {
        if let Item::Fn(func) = item {
            let mut tails = Vec::new();
            tail_matches(&func.body, &mut tails);
            for e in tails {
                match_case_try_as_case(e, src, as_case.as_deref(), &mut hits);
                let_else_as_case(e, src, as_case.as_deref(), &mut hits);
            }
        }
    }
    each_expr(program, &mut |e| match_map_err(e, &mut hits));
    each_expr(program, &mut |e| match_map_ok(e, &mut hits));
    each_expr(program, &mut |e| field_init_shorthand(e, src, &mut hits));
    each_expr(program, &mut |e| set_spread_field(e, src, &mut hits));
    // The array sibling of the above, and disjoint from it by the kind of
    // literal the `set` rebuilds from: a struct there, an array here.
    let appends = append_names(program);
    each_expr(program, &mut |e| {
        set_spread_push(e, src, &appends, &mut hits)
    });
    // Only where this file's `push` is the builtin — see `pipeline_names`,
    // which also reports what `map`/`filter` are called here.
    let pipeline = pipeline_names(program);
    each_expr(program, &mut |e| {
        push_loop_pipeline(e, src, &pipeline, &mut hits)
    });
    // The optional-guard sibling of the pipeline lint: an `if let some(..)` where
    // that one reads a boolean `if`.
    let collect_ok = collect_ok_names(program);
    each_expr(program, &mut |e| {
        push_loop_collect_ok(e, src, &collect_ok, &mut hits)
    });
    // The set-valued sibling of the pipeline lint, and disjoint from it: that
    // one's seed is an empty *array* filled by `push`, this one's an empty set
    // grown by `union`.
    let unions = union_names(program);
    each_expr(program, &mut |e| {
        union_loop_union_all(e, src, &unions, &mut hits)
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
    // The same loop shape folded to a single yes/no instead — disjoint from
    // both of the above, which hand back an element or its index rather than a
    // bool literal.
    let any_all = any_all_names(program);
    each_expr(program, &mut |e| {
        return_loop_any_all(e, src, &any_all, &mut hits)
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
        each_expr(program, &mut |e| step_by_one(e, src, &steps, &mut hits));
    }
    // The general form of the above, and partitioned against it rather than
    // overlapping: `set x = x + 1;` belongs to the step lint whenever that lint
    // is live here, so `compound_assign` is told and leaves that one shape
    // alone. Only the operators whose compound spelling provably means the same
    // thing are advised — see `matching_compound`.
    let compound = matching_compound(program);
    if !compound.is_empty() {
        each_expr(program, &mut |e| {
            compound_assign(e, src, &compound, &steps, &mut hits)
        });
    }
    // The same accumulate shape for `+++`, whose in-place form is a method
    // rather than a compound operator. Disjoint from the above by operator, and
    // only where `+++` is the builtin `concat` — see `extend_names`.
    let extends = extend_names(program);
    if !extends.is_empty() {
        each_expr(program, &mut |e| extend_longhand(e, &extends, &mut hits));
    }
    // Only for the comparisons this file's `<`/`>` really are — see
    // `len_zero_cmp`, which also says whether `is_nonempty` needs importing.
    let cmp = len_zero_cmp(program);
    each_expr(program, &mut |e| len_gt_zero(e, src, &cmp, &mut hits));
    // `x.value_or(d) == lit` — same import caution for `==`/`!=`.
    let filled = value_or_compare_names(program);
    each_expr(program, &mut |e| {
        value_or_compare(e, src, &filled, &mut hits)
    });
    // The mirror of the above: `<`/`>` against zero go to `is_nonempty`, `== 0`
    // and the negated predicate go to `is_empty`. Same import caution.
    let empties = empty_names(program);
    each_expr(program, &mut |e| {
        is_empty_longhand(e, src, &empties, &mut hits)
    });
    // `xs.with(i, v).value_or(xs)` is `xs.try_set(i, v)`; only where `with` and
    // `value_or` are the builtins — see `with_names`, which also names the
    // `try_set` import when it is missing.
    if let Some(names) = with_names(program) {
        each_expr(program, &mut |e| {
            with_value_or_self(e, src, &names, &mut hits)
        });
    }
    // The lints ported to AIPL ran with the parse: `unused_imports`,
    // `duplicate_import_source`, `destructure_binding`, `destructure_param` and
    // `fn_body_type_stutter`, so far.
    hits.extend(ported);
    hits.retain(|e| match &e.span {
        Some(sp) => {
            let line = line_of(src, sp.start);
            if allowed.contains(&line) {
                squelched.insert(line);
                false
            } else {
                true
            }
        }
        None => true,
    });
    // Every hit that survives says how to keep it, so no lint spells that out in
    // its own message. A hit with no span has no line for a marker to go on.
    for hit in hits.iter_mut().filter(|e| e.span.is_some()) {
        hit.message.push_str(SQUELCH_ADVICE);
    }
    // A marker is an acknowledgement of a lint that fired, so one with nothing to
    // squelch is stale: the code was fixed, or the marker was never needed. It is
    // not itself squelchable — what it would squelch is itself.
    for sp in allows {
        if !squelched.contains(&line_of(src, sp.start)) {
            hits.push(Error::at(
                "this `#[allow]` squelches nothing — remove it",
                sp.clone(),
            ));
        }
    }
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
        if !matches!(decl.source, ImportSource::Builtins) {
            return None;
        }
        decl.names
            .iter()
            .find(|n| n.name == builtin)
            .map(|n| n.local().to_string())
    })
}

/// Whether `e` can be lifted into a rewritten pipeline's lambda: it must be
/// [`lambda_safe`], and it may not mention `acc` — the binding being
/// accumulated — because the rewrite no longer has it (`out.len()` as a running
/// index is the shape this catches).
///
/// Shared by the two accumulate-in-a-loop lints, which have to agree on what a
/// liftable expression is: they differ only in what the loop does with it.
fn liftable(e: &Expr, acc: &str) -> bool {
    if !lambda_safe(e) {
        return false;
    }
    let mut ok = true;
    crate::each_subexpr(e, &mut |x| {
        if matches!(&x.kind, ExprKind::Ident(n) if n == acc) {
            ok = false;
        }
    });
    ok
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

/// The binding a `set` target is rooted in: `p` for `p` and for `p.a.b`, and
/// `None` for anything that is not a place. Shared by the two `set`-rebuild
/// lints, which ask it the same two questions: what binding is being rebuilt,
/// and is the target spellable back into the advice at all.
fn root_name(e: &Expr) -> Option<&str> {
    match &e.kind {
        ExprKind::Ident(n) => Some(n),
        ExprKind::Field(recv, _) => root_name(recv),
        _ => None,
    }
}

/// Whether `e` mentions `name` as a bare identifier anywhere. Asked by every lint
/// whose rewrite drops a binding or
/// splits one statement into several — either way the question is whether some
/// sub-expression still refers to it.
fn mentions(e: &Expr, name: &str) -> bool {
    let mut found = false;
    each_subexpr(e, &mut |x| {
        if matches!(&x.kind, ExprKind::Ident(n) if n == name) {
            found = true;
        }
    });
    found
}

/// 0-based line number of byte offset `pos` in `src`.
fn line_of(src: &str, pos: usize) -> usize {
    src[..pos.min(src.len())].matches('\n').count()
}

/// The synthetic value the parser ends a statement-only block with: `()` for an
/// ordinary block, and an i64 `0` — the loop expression's own result — for a
/// loop body, which may not end in a value of its own at all. The loop's zero is
/// told from a written one by its empty span, the same way
/// `slice_from_zero` (in AIPL) tells an elided slice bound from a
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

/// The source text that spells `e`, or `placeholder` when its span doesn't
/// cover it. [`spans_its_text`] settles the name and field-path forms; a
/// literal is added on top of them here, and *checked* against the source
/// rather than assumed, because the parser also synthesizes literals carrying
/// the span of whatever desugared into them — the `1` an `++` expands to holds
/// the `++`'s own span, and splicing that back would quote the wrong text. A
/// literal that fails its check simply falls back to the placeholder.
fn quote<'a>(e: &Expr, src: &'a str, placeholder: &'a str) -> &'a str {
    let text = src.get(e.span.clone()).unwrap_or("");
    let spells_it = match &e.kind {
        ExprKind::Num(n) => text.parse::<i64>() == Ok(*n),
        ExprKind::Bool(b) => text == if *b { "true" } else { "false" },
        ExprKind::Char(_) => text.len() >= 3 && text.starts_with('\'') && text.ends_with('\''),
        // A `"""` block spells itself too — it is raw source either way — but
        // only while it stays on one line, which is all a message can hold.
        ExprKind::Str(_) => {
            text.len() >= 2 && text.starts_with('"') && text.ends_with('"') && !text.contains('\n')
        }
        _ => spans_its_text(e),
    };
    if spells_it {
        text
    } else {
        placeholder
    }
}
