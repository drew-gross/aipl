# Speeding up `inline_single_use_post_mono`

## Context

`aipl check crates/aipl-codegen/src/lint/lint.aipl` takes ~24s. A flamegraph
(`OUT=target/flamegraph/lint.svg scripts/flamegraph_check.sh <file>`) puts 95% of it in
`compile_program`, 64% in the JIT, and 17% of the whole run in
`inline_single_use_post_mono` (`crates/aipl-mono/src/lib.rs`). No other optimizer pass is above 4%.

The pass is a loop that inlines one single-use function per round and starts over. Each round:

- `use_counts_mono` walks every body (10% of the run),
- `collect_binders_mono` walks every body (5%),
- `find` looks for the next candidate, running `is_inline_shape_typed` on every single-use
  function that is not eligible, because only an inlined one joins `skip`,
- `inline_typed::expand_calls` runs `references_name` over every body and then `replace_call` over
  those that mention the function (rewrites in place, so no longer significant).

The rounds are one per inlinable function (lifted lambdas and single-caller instances), so the work
is the number of bodies times their size, squared.

## Committed decisions

- The pass's output must not change: the same functions are inlined in the same order, so no
  baseline row, IR or `--- report ---` moves. Every stage is verified by a green handoff with no
  baseline changes, and by the flamegraph.
- `binders` stays exact. Inlining renames the parameters to fresh `$inl…` names, so a name can stop
  being bound; keeping it in the set would stop a function of that name from being inlined, which
  changes the output. Maintain it as a per-name count rather than a set that only grows.

## Stages

### 2. Keep use counts and binders across rounds

Goal: one walk of the program for the whole loop instead of one per round.

- [ ] Inlining and eliding `f` leaves every other count unchanged (the body moves); drop `f`'s
  entry. When `f` stays (`library` or `externally_called`) its body is copied, so add the counts of
  the names the body uses.
- [ ] `binders` as a name → count map: the removed parameters' names go down by one, the fresh
  names are not functions and need no entry.
- [ ] A test that runs the pass on a program with a chain of single-use functions and compares the
  result with the pass recomputing from scratch each round (keep the slow version in the test).

### 3. Find the call site without walking every body

Goal: `references_name` stops being a walk of the whole program per round.

- [ ] Index name → the functions that mention it, built once and updated for the one function
  `expand_calls` rewrote (its body gains the names `f`'s body used).
- [ ] Functions that fail the shape check join `skip`, so `find` stops re-testing them each round.

Stages 2 and 3 share the loop's state; take them together if the state is awkward to split.

`replace_call` mutates in place; `rename_scoped` still clones per node but is 0.1% of the run.

## Done when

A flamegraph of `aipl check crates/aipl-codegen/src/lint/lint.aipl` shows `inline_single_use_post_mono`
below 5% of the run, and handoff is green with no baseline row, IR or report moved.
