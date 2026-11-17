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

- [x] `hoist_test_fns.aipl` makes `__test$f` from `test_body` (`with_test_function`: public, no
      parameters, effects `measure` for a benchmark and none otherwise, the carrier of a
      top-level `test({ .. })` dropped), after `f`, the helpers and the benchmark function as
      before. `Function` coming out of it has `test_body: none`.
- [x] `ffi_ast.rs` no longer decodes `test_body` or `test_fns`; `ast::Function` and
      `TFunction` have neither. The conversion Round A put in the loader is gone.
- [x] The loader's `test_body` handling is gone: operators are gated in the test functions as in any
      function, `bake.rs` treats a `TestBody` role as test code, and a file that is not the
      root keeps none of its test code (`is_test_code`, by role).
- [x] The mono passes (`passes`, `hygiene`, `fold`, the inliners, `instantiation`,
      `lower_reductions`, `grid_literals`, `typed_util`) carry no `test_body`.
- Bootstrap: the new decoder ignores the AIPL fields, so with the old artifact a `.test`
  block is silently dropped until the staged IR is promoted, which `cargo handoff` does
  before it runs anything that counts tests.
- Watch: hoisted names and spans, reported names, `fn_names.aipl` roles, baseline rows.

### Round C: the stage in the types

Done as built, which differs from the sketch in three ways: `Program` is not generic (a
refinement does not survive a generic struct's instance: `mangle_type` drops it, so
`Program<WrittenItem>` and `Program<Item>` would be one struct), the lowered case is
`LoweredFn` (a refinement leaves a case out by name, anywhere it can be, and `Fn` is also a
case of `Ty` and others), and the program types are `throughout` refinements of one struct.

- [x] `Function` loses `test_body`, `test_fns`, `benchmark`; `Attributes { test_body, test_fns,
      benchmark }` carries them, and its helpers are plain `Function`s.
- [x] `variant SuperItem = WrittenFn(Function, Attributes) | LoweredFn(Function) | Struct ..`;
      `WrittenItem = SuperItem without LoweredFn`, `Item = SuperItem without WrittenFn`;
      `struct SuperProgram`, `WrittenProgram = SuperProgram without LoweredFn throughout`,
      `Program = SuperProgram without WrittenFn throughout`.
- [x] The parser builds `WrittenFn` (`ast.aipl`'s `Function` node carries the attributes);
      `hoist_test_fns: WrittenProgram -> Program`; `promote_type_vars`, `desugar` take and
      return `Program`; `with_source_text` takes and returns `WrittenProgram`.
- [x] The lints take `WrittenProgram`/`WrittenItem[]`, and `code_bodies` matches `WrittenFn`
      only. `aipl_parse_file` is the lowered parse (`LoweredFile`); `aipl_parse_surface_file`
      the written one.
- [x] Checker: a refinement inside `ok`/`err`/`some`, through blocks, `if`s and `match`es; a
      `Case<V>` ignores its variant's refinement; a bound ignores the refinement of the type
      it is checked on; a shallow exclusion is as good as `throughout` where nothing the
      variant carries can hold the case.
- Bootstrap: the decoder reads `Fn` as well as `LoweredFn` for one round; remove `Fn` once
  the artifact is promoted.

### Round D: tidy

- [x] The transitional `Fn` in the decoder is gone; `LoweredFn` is the only function case.
- [x] The comments describing the stages are current (`parse_and_partial_load.aipl`'s header,
      `item.aipl`, `ffi_ast.rs`).
- [x] The finished item is out of `CLAUDE_TODO.txt`.
- No `WrittenFn` arm is carried by a pass that sees only `Item`: the narrowing made them
  unwritable.

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
