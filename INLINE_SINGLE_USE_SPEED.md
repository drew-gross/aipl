# Speeding up `inline_single_use_post_mono`

## Context

`aipl check crates/aipl-codegen/src/lint/lint.aipl` takes ~24s. A flamegraph
(`OUT=target/flamegraph/lint.svg scripts/flamegraph_check.sh <file>`) puts 95% of it in
`compile_program`, 64% in the JIT, and 17% of the whole run in
`inline_single_use_post_mono` (`crates/aipl-mono/src/lib.rs`). No other optimizer pass is above 4%.

The pass is a loop that inlines one single-use function per round and starts over. Use counts and
binder multiplicities are kept across rounds (`SingleUseTally`: a sum of per-function
contributions, updated for the one caller rewritten and the one function removed). Each round:

- `find` looks for the next candidate, running `is_inline_shape_typed` on every single-use
  function that is not eligible, because only an inlined one joins `skip`,
- the caller is found by `references_name` over every body, then `replace_call` rewrites it in
  place.

The rounds are one per inlinable function (lifted lambdas and single-caller instances), so the work
is the number of bodies times their size, squared.

## Committed decisions

- The pass's output must not change: the same functions are inlined in the same order, so no
  baseline row, IR or `--- report ---` moves. Every stage is verified by a green handoff with no
  baseline changes, and by the flamegraph.
- `binders` stays exact. Inlining renames the parameters to fresh `$inl…` names, so a name can stop
  being bound; a set that only grows would stop a function of that name from being inlined, which
  changes the output. `SingleUseTally` holds a per-name count.

## Stages

### 3. Find the call site without walking every body

Goal: The caller search stops being a walk of the whole program per round.

- [ ] Index name → the functions that mention it, built once and updated for the one function
  rewritten (its body gains the names `f`'s body used); `SingleUseTally` is where it would live.
- [ ] Functions that fail the shape check join `skip`, so `find` stops re-testing them each round.

`replace_call` mutates in place; `rename_scoped` still clones per node but is 0.1% of the run.

## Done when

A flamegraph of `aipl check crates/aipl-codegen/src/lint/lint.aipl` shows `inline_single_use_post_mono`
below 5% of the run, and handoff is green with no baseline row, IR or report moved.
