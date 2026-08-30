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

mod eta_lambda;
mod field_init_shorthand;
mod fn_body_type_stutter;
mod incr_by_one;
mod len_gt_zero;
mod match_is_some_and;
mod match_map_err;
mod match_map_ok;
mod match_value_or;
mod push_loop_pipeline;
mod slice_from_zero;
mod slice_to_len;
mod slice_whole;
mod unused_imports;

use crate::ast::{Expr, ExprKind, Program};
use crate::{each_expr, Error, Span};
use std::collections::HashSet;

use self::eta_lambda::eta_lambda;
use self::field_init_shorthand::field_init_shorthand;
use self::fn_body_type_stutter::fn_body_type_stutter;
use self::incr_by_one::{incr_by_one, matching_increment};
use self::len_gt_zero::{len_gt_zero, len_zero_cmp};
use self::match_is_some_and::match_is_some_and;
use self::match_map_err::match_map_err;
use self::match_map_ok::match_map_ok;
use self::match_value_or::match_value_or;
use self::push_loop_pipeline::{pipeline_names, push_loop_pipeline};
use self::slice_from_zero::slice_from_zero;
use self::slice_to_len::slice_to_len;
use self::slice_whole::slice_whole;
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
    each_expr(program, &mut |e| match_map_err(e, &mut hits));
    each_expr(program, &mut |e| match_map_ok(e, &mut hits));
    each_expr(program, &mut |e| field_init_shorthand(e, src, &mut hits));
    // Only where this file's `push` is the builtin — see `pipeline_names`,
    // which also reports what `map`/`filter` are called here.
    let pipeline = pipeline_names(program);
    each_expr(program, &mut |e| {
        push_loop_pipeline(e, src, &pipeline, &mut hits)
    });
    // Only where a `++` flavor provably matches this file's `+` — see
    // `matching_increment`, which also names the import when it's missing.
    if let Some(incr) = matching_increment(program) {
        each_expr(program, &mut |e| incr_by_one(e, incr, &mut hits));
    }
    // Only for the comparisons this file's `<`/`>` really are — see
    // `len_zero_cmp`, which also says whether `is_nonempty` needs importing.
    let cmp = len_zero_cmp(program);
    each_expr(program, &mut |e| len_gt_zero(e, src, &cmp, &mut hits));
    unused_imports(program, &mut hits);
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
