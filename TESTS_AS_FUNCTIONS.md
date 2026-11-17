# Tests and benchmarks as functions, and a program that says which stage it is at

Plan, with checkboxes. Each round ends at a green `cargo handoff` and stops there.

## Why

A function carries `.test` and `.benchmark` attributes as it is written. Hoisting turns part
of that into ordinary functions and leaves the rest attached, in two places that both do some
of the work:

- `hoist_test_fns.aipl` moves a `.test` block's helper functions out as `__test$f$helper` and a
  `.benchmark` block out as `__benchmark$f`. The `.test` block itself stays on the function as
  `test_body`, and the benchmark function carries its block as `test_body` too.
- `synthesize_driver` (`aipl-codegen/src/lib.rs`) hoists *again*: every function with a
  `test_body` gets a `__test$f` function made from the block, and loses the `test_body`.

So the type that is supposed to mean "a regular function" (`Function`) can still carry
`test_body`, `test_fns` and `benchmark`, and which of them is empty depends on how far a
program has got. Nothing checks it.

The goal is for that to be the type's job:

```
variant SuperItem = WrittenFn(Function, Attributes) | Fn(Function) | Struct(..) | Variant(..)
                  | Const(..) | Refinement(..) | Import(..)

variant WrittenItem = SuperItem without Fn            // what the parser produces
variant Item        = SuperItem without WrittenFn     // what hoisting produces, and everything after
struct  Program<I>  { items: I[] }
```

- `Function` has no attribute fields. A `.test` helper is a `Function`, so it cannot hold a block.
- `hoist_test_fns: Program<WrittenItem> -> Program<Item>`. The lints take `Program<WrittenItem>`
  and never meet a `Fn`; the passes after hoisting take `Program<Item>` and carry no `WrittenFn`
  arm.
- Code that works at either stage (`show_program`, `item_names`) takes plain `SuperItem`.

`Program` is generic because `without Fn throughout` would also exclude the helpers held
inside a `WrittenFn`'s attributes, which are `Function`s.

## What the Rust side does with `test_body` today

It is how an attached block reaches Rust, in three forms: a function's own `.test`, a
`__benchmark$f` carrying its block, and a top-level `test({ .. })` carried by a
`__test_block_N` function.

| where | what it does |
|---|---|
| `aipl-loader/src/lib.rs` | gates operators in it against the file's imports; rewrites globals in it for the root file only; `without_tests` drops it (and the benchmark function) for an imported file |
| `aipl-loader/src/bake.rs`, `kwargs.rs` | bakes `assert`/`trace` in it as test code; expands keyword arguments in it |
| `aipl-mono/src/pin.rs` | `pin_test_body` types it as a function the test driver makes; `in_test` is also set by name (`is_test_body`) |
| `aipl-codegen/src/lib.rs` | `synthesize_driver`: makes `__test$f` from it, strips it from the original, builds `__test_main` and the reported names; `build_benchmark_program` looks for it |
| `aipl-mono` passes (`passes.rs`, `hygiene`, `fold`, the inliners, `instantiation`, `lower_reductions`, `grid_literals`) | carry it along and rewrite it like a body |
| `aipl-syntax/src/lib.rs` | `trace_diagnostics`, `benchmark_diagnostics`, and the generic walkers read it |
| `src/main.rs`, `tests/suites/pin.rs` | whether a file has tests |

## Rounds

### Round A: the Rust side hoists once, at the end of loading

One place turns `test_body` into functions, once the loader has finished with a file, so
nothing after the loader sees `test_body`. Still Rust; no AIPL change; no bootstrap. The
loader's own handling of `test_body` (operator gating, rewriting, baking, keyword arguments,
dropping an imported file's) stays until Round B, when the field goes, so that the compiler
finds every use.

- [x] `aipl_syntax::hoist_test_bodies` makes the `__test$f` function (public, no parameters,
      the block as its body, effects `[]` for a test and `["measure"]` for a benchmark) and
      drops a top-level test's carrier; the loader applies it to the merged items.
- [x] `synthesize_driver` selects by role (`test_body_source_name`, `benchmark_source_name`)
      and builds the driver; reported names come from the source name.
- [x] `benchmark_diagnostics`, `build_benchmark_program` and `main.rs`'s `has_tests` read the
      test functions.
- [x] The pinner no longer types an attached block: `pin_test_body` is gone, and a test is
      pinned as the function it is. `display` names `__test$f` as `f`, so a message about a
      test block reads as before.
- [ ] Moved to Round B: deleting the downstream `test_body` handling (the mono passes,
      `check_operators`/`rewrite_expr`/`kwargs` special cases, `ast::Function.test_body`), which
      is simplest when the field itself is removed.
- Watch: test error wording and carets, `check`'s per-test rows, `trace` refusal inside a
  test, `assert` baking, the LSP, `perf.rs`, baseline rows for programs with tests.

### Round B: AIPL hoists the `.test` block; Rust drops `test_body`

- [ ] `hoist_test_fns.aipl` makes `__test$f` from `test_body` (and keeps the helpers and the
      benchmark function as it does), so `Function` coming out of it has `test_body: none`.
- [ ] `ffi_ast.rs` stops decoding `test_body`, `test_fns` and `benchmark`. Bootstrap: the new
      decoder reads the old artifact's fields for one round, then the fallback goes.
- [ ] Delete Round A's Rust conversion and `ast::Function.test_body`, `test_fns`.
- Watch: the hoisted names and spans (`__test$` functions point at the block, not at the
  function), reported names, `fn_names.aipl` roles.

### Round C: the stage in the types

- [ ] AIPL `Function` loses `test_body`, `test_fns`, `benchmark`; `Attributes { test_body,
      test_fns, benchmark }` carries them; `variant SuperItem` as above, `Fn` kept as the
      lowered case's name so downstream arms do not change.
- [ ] `struct Program<I>`; `WrittenItem` and `Item` aliases; the parser returns
      `Program<WrittenItem>`.
- [ ] `hoist_test_fns` takes and returns the two stages; its non-function arm is the narrowing
      `_` that gives `Item`.
- [ ] Lints take `Program<WrittenItem>`: `code_bodies` matches `WrittenFn` only (drops the
      attribute-less special case in `bodies`); `item_names`/`function_names` take `SuperItem`.
- [ ] `promote_type_vars`, `desugar`, `desugared_item` and the decoder take `Program<Item>`;
      the decoder's `Fn` case is the only function case.
- Watch: anything that builds a `Function` (the parser, `standing_in`, `benchmark_function`,
  `hoisted`) loses fields; the written-stage formatter output must not change.

### Round D: tidy

- [ ] Delete what the types made dead: the `WrittenFn` handling in passes that only see `Item`.
- [ ] Update the comments that describe the stages (`parse_and_partial_load.aipl`'s header,
      `item.aipl`'s `Function` docs, `ExprKind`'s) and the docs site if it mentions the order.
- [ ] Remove the `without` items this finishes from `CLAUDE_TODO.txt`.

## Open questions

- **A benchmark's name.** Today `__benchmark$f` carries the block as its `test_body`, and the
  driver makes `__test$__benchmark$f` from that. With the block as a function body, is the
  benchmark function `__benchmark$f` itself (role `Benchmark`, declares `measure`), with the
  driver calling it directly? Reported names must stay `f.benchmark`.
- **A top-level `test({ .. })`.** It is a `__test_block_N` function that exists only to carry
  the block. After Round B it should just be the test function (role `TestBody`), with no
  carrier.
- **Test errors.** A type error in a test block is reported with the block's source today.
  As a function it would be `fn "__test$f"`; the fixtures that pin this will change wording
  unless the renderer maps the name back. Decide in Round A.
- **Roots.** `pin_function` types every non-generic function, so test functions are checked
  without being reachable from `main`; confirm in Round A that generic functions only used by
  tests still get checked as they are today.
- **`aipl run` / `build`.** One place excludes test-role functions; the effect checker and
  the dead-code passes must not see them. Confirm in Round A.

## Done when

- `Function` has no attribute fields and the rest of the compiler has no `test_body`.
- `hoist_test_fns` has the types above and nothing after it can hold a `WrittenFn`.
- The corpus, `check` output and the baseline move only where a test is a function now.
