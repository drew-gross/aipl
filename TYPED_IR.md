# Typed IR: a pinned, fully typed program between the checker and every optimization

Multi-session project. Update the checkboxes and the "Status / notes" section at the end of each session.

## Why

Optimizations keep stopping because the expression they hold has lost its type.

- `check` (`crates/aipl-mono/src/check.rs`, ~8k lines) is deliberately permissive. A generic call whose type
  variables are not instantiated yields `Type::Unknown`, and the only types it keeps are stamped on
  *context-dependent* expressions (`Expr::ty`: bare `none`, `[]`, `ok`/`err`, narrowed int array literals).
- Every pre-mono pass (`inline_*`, `fuse_operations`, `counting_variants`, `fold_lengths`, `appending_variants`,
  `fold_extends`, `fold_constants`, `sink_bindings`, `fuse_try_with`; run by `aipl_mono::passes::optimize` from
  `lower_program`, `crates/aipl-codegen/src/lib.rs` ~3010-3260) works on syntax and **guesses types**:
  `fold_extends::ArrayReceivers` (`holds_array`, `known_array`), the `opt.map(f)` vs `xs.map(f)` ambiguity,
  `narrows_literal_array`, `is_unpinned_optional`.
- Only `monomorphize` -> `infer` (`aipl-mono/src/lib.rs` ~6595) pins types, per instance, after the pre-mono
  pipeline has run. Post-mono passes see `ConcreteFn { body: Expr }` with `Expr::ty` mostly `None`, and
  `ConcreteType` still has `NoneInner`, `EmptyArrayArg`, `NoneLiteralArg`, `ConcatStr`.

## Goal

A type-pinning pass right after lowering, with **no structural change** to the program, that produces a
*separate Rust type* that cannot represent a flexible int, an uninferred expression, `Unknown`, `NoneInner`, etc.
Every later stage (all optimizations, fusions, monomorphization) runs on that type, so a type can never be lost.

## Decisions (settled with Drew)

1. Pin **before mono, generic bodies included**: types may name a rigid type variable `T`; every generic call
   records its type arguments; mono becomes substitution over pinned IR, not re-inference.
2. **The pinner *is* the type checker.** One engine replaces `check.rs`, producing diagnostics and the pinned IR
   in the same walk. Every `--- check --compile ---` / `--- check --tests ---` section may change and is refilled.
3. **Unconstrained types fall back to `i64`**, the way an integer literal does: `[].len()` is an `i64[]`, a
   context-free `none` is an `i64?`, `#{}` settles on a set of `i64` (today's set-over-dict rule is kept). The
   value is empty, so codegen is unchanged. No new error; no uninferred form survives. An *ambiguous generic call*
   (variable neither pinned by arguments nor defaultable) is still an error.
4. The migration is **staged so the tree is green and hand-off-able after every stage**, not one big bang. The
   old checker and old pipeline stay compiled and default until the stage that replaces them. (Reason: no git
   operations are allowed in sessions, so a half-ported red tree has nothing to fall back to.)

## Design

### The pinned IR (`crates/aipl-syntax/src/typed.rs`; parser/loader/fmt/lsp/index stay on `ast`)

Generic over the type representation so the same tree serves both sides of mono and substitution is a type map:

```
TExpr<T> { kind: TExprKind<T>, ty: T, span, value_span, name_span }   // ty mandatory on EVERY node
Ty         // pre-mono. Unit, Primitive, Named, Case, Var(String) (rigid), Optional, Array, Set, Dict, Result, Fn
ConcreteTy // post-mono. Same minus Var.
```

Not representable: `Unknown`, `Any`, `NoneInner`, `EmptyArrayArg`, `NoneLiteralArg`, `ConcatStr` (a mono-time
representation choice, stays a `ParamSpec` flag), `Without`, `Tuple`, `Generic`. Sets carry a `PinnedOrder` (no
`Context`). Literals are `Int(i64)` / `Float` typed by `ty`. `TExprKind` mirrors `ExprKind` minus `KwArg`,
`Spread`, `TypeArgs`, `TupleLit`, and `Call` carries `type_args`. `IfLet` stays (no structural transforms).
`concretize` maps `Ty -> ConcreteTy` totally (None if a `Var` is unbound); `erase` goes back to `ast`.

### The pinner (`crates/aipl-mono/src/pin.rs`)

Unification-based, bidirectional (pushes context down as `flex_fit` does so a literal takes its width from where
it sits). Fresh inference variables for `[]`, `none`, `ok`/`err` placeholders, generic-call type parameters and
integer literals. Builtins are ordinary functions from `native_builtin_decls` + the AIPL-implemented builtins.
At function end, zonk and default every unresolved variable to `i64` (once, at the end, never earlier).
Diagnostics are ported rule by rule from `check.rs` (its rules are the spec); texts may change. Refinements are
checked here and erased as `erase_refinements` does today.

### Mono over pinned IR

`infer` (the inference half) goes away. What stays, now as `TExpr<Ty> -> TExpr<ConcreteTy>`: substitution of
`Var`, lambda lifting, per-parameter `ParamSpec` specializations (`owned`, `str_kept`, `set_kept`, `concat`,
`variadic`, `drop_none`), `mutating` desugar, `lower_for`, `IfLet` -> `Match`.

### Passes

The pass manager is already generic (`Pass<P>`, `Optimizable`). Port all pre-mono (`TProgram`) and post-mono
(`MonoProgram`) passes to `TExpr`; shared traversal helpers become one generic set. Then delete the type guesses.
Any pass that builds a node must supply its `ty` (the type system enforces it); new nodes copy the type of the
node they replace.

## Design amendments found while building stage 1

The pinner is *unification plus coercion sites*, not pure unification. The checker's `coerce` (`check.rs`,
"the specification") is structural and covariant, with a short list of aliases: an integer widens to a wider
declared one (only for a scalar, never inside a container), a `str` is a `char[]` (and pins an open element to
`char`), `Error` is a `str`, a `Case<V>` is its tag, a refinement is its base, and an empty `#{}` is any set or
any dict. These live in `Pinner::agree`/`Pinner::coerce`, applied wherever a value meets a declared type
(argument, return, annotated `let`, field, constructor payload, assignment). Builtins also carry their own rules
that no signature expresses (`contains`/`starts_with`/`extend` take a sequence, one element or an optional;
`len`/`is_empty` take sets, dicts and spans; `map` is re-signed for optionals and spans; `to_set`/`narrow`/
`to_float` take their result type from the position they sit in). Each is a special case in `Pinner::call`,
found by running the oracle and reading what it rejects.

Consequences for the types in `typed.rs`:
- `Ty::Generic(base, args)` exists for `Box<T>` inside a generic body. An application whose arguments are all
  concrete resolves to the instance's name (`Box$i64`, `generic_instance_name`), exactly as lowering spells it, so a
  type has one spelling. `ConcreteTy` has no such case; `Ty::concretize_with` takes the instance namer.
- Anonymous `any`s become type variables `any$N` of the function, appended to its `type_vars` (and to a call's
  `type_args`, after the declared ones). They are not rigid-equal to each other.
- An unconstrained set order, empty `#{}`, `none`, `[]` and integer/float literals are inference variables (own
  `Ord` and `Kind::{Int, Float, Brace}`), defaulted at function end.
- A `Return` expression has no value. It gets a fresh variable marked as diverging: where context asks for a
  type (a branch of an `if` whose other branch yields a number) it takes that type; where nothing does (`return x;`
  as a statement) it settles on `Ty::Never` / `ConcreteTy::Never`, not on the default number. The written
  language has no such type, so `Never` erases to `()`.
- A `.test`/`.benchmark` block is pinned as the function the test driver makes of it: no parameters, no type
  variables, no effects (a benchmark has `measure`), a unit result, and `?` fails the test instead of returning.
  The checker only looks at a block when tests run, so the oracle holds the pinner's verdict on a block to the
  checker's verdict on `build_test_program`/`build_benchmark_program`. A block that fails to pin is reported as
  `<fn>.test` and the function itself still pins.
- A parameter default sees only the parameters before it and is pinned against its parameter's type, except when
  that type mentions a type variable (`pad: T = 0`): there it is checked on its own and left out of the pinned
  signature, because a call is what decides `T`. Struct field and variant payload defaults are pinned the same
  way; those of generic declarations are left out. Variants are now items of the pinned program.

## Running the oracle

`cargo test --test compiler -- --ignored pin::oracle_report --nocapture` loads every `.aipl` under `tests/cases/`
and `crates/`, runs the checker's `frontend`, pins what it accepted, and prints pinned / gap / rejected counts
with the commonest gaps and rejections, each with an example file and function. `AIPL_CASE=a,b` narrows it (a
path substring list; a few directories take seconds, the whole corpus about three minutes). Counts are per
program, and every program carries the builtins and any compiler sources it imports, so the same function is
counted many times: read the *ranking*, not the totals. A "rejected-but-checker-accepted" line is a missing
coercion or special case; a "gap" is a form not yet typed.

The other direction is measured too: for a program the checker rejects (`frontend` returns `Err`), the harness
pins the lowered program from `lowered_for_check` and counts a miss when the pinner accepts it.

## Stages (each ends green: targeted tests, then one `cargo handoff`)

- [x] **0. `typed.rs`**: `Ty`, `ConcreteTy`, `TExpr`, `TProgram`, `erase`, `concretize`; 6 unit tests. Additive.
- [x] **1. `pin.rs` as an oracle, beside the old checker.** Nothing consumes it. Agreement over the whole corpus
      reached over the whole corpus (see Status), including `.test` bodies, defaults and `Never`.
  - [ ] Declarations: structs, variants, generic templates, signatures -> `TSignature`/`TItem`.
  - [ ] Unifier core: inference variables, occurs/rigid-var handling, zonk, `i64` defaulting.
  - [ ] Expression rules, in rising difficulty: literals/idents/let/mut/set/if/while/for; calls to non-generic
        functions; construct/field/match/IfLet; generic calls + `type_args`; lambdas; builtin special rules
        (optional-vs-array `map`, spans, variadics, `narrow`, `to_set`, `collect_arms`, `as_case`); `?`; shim.
  - [ ] Diagnostics ported from `check.rs`.
  - [ ] Test harness: over *every* corpus program (cases + `crates/**/*.aipl`), the old checker's accept/reject
        must agree with the pinner's; `erase(pin(p)) == p`; the fallback cases (`[].len()`, context-free
        `none`, `#{}`, unused `ok(1)`) with equal `--- report ---` to the `i64`-annotated spelling.
  - Exit: agreement over the whole corpus, with a known-gaps list that is empty. Handoff green (pinner unused).
- [ ] **2. Monomorphize over pinned IR**, in steps that each leave the tree green (a rewrite of `Mono::infer`,
      ~1,600 lines that interleave inference with lambda lifting, ownership, variadic shapes and desugaring,
      cannot be swapped in one piece, and the pre-mono passes sit between pinning and mono, so a pinned tree
      does not reach mono intact yet):
  - [x] **2a. Agreement probe** (`probe.rs`, `codegen::mono_agreement`, `pin::mono_agreement_report`): monomorphize
        the unoptimized program and compare `infer`'s type for each expression with the pinned one.
  - [x] **2b. Pinned types replace the checker's stamps.** Decision (Session 6): `frontend` stamps every node the
        pinner typed (except unit, never and defaulted nodes); 166 baseline rows moved as their own measured change.
  - [x] **2c. `infer` reads the stamps.** (Session 10.) A node's type is its stamp unless there is none, the stamp
        holds a placeholder, or the node is a call whose arguments decide its type (`result_only`); `narrows_literal_array`
        and the special arms for literal widths, `to_float` and `collect_arms` are gone, one rule replaces them. What made
        this possible: (a) the program is **re-pinned after the pre-mono passes** (`codegen::repin`,
        `Stamping::Replace`), so a stamp can no longer be stale however the passes moved nodes; (b) a stamp spells a
        generic instance as the application it is (`Pinned::instances`, recorded when the pinner names an instance),
        and `infer` instantiates it with `resolve_generic_ty`; (c) the pinner types a constructor name beside a
        `Case<V>` in `==` as that case. Four baseline rows moved (listed in Session 10 notes).
        Still to delete, because each needs a call's `type_args` (2d): `Num` flexing through `flex_int_ty`,
        `drop_none`/`is_unpinned_optional`, `drop_lit`.
  - [x] **2d. The last guesses are gone** (Session 11). Reframed like stage 3: no `TExpr` substitution is needed, because
        every node's stamp is its type, so a generic call's type arguments are read off the argument and result
        stamps by the one binding routine (`instantiate_types`). Deleted: the deferred-literal pass and
        `flexible`, `lit_pinned`, `ParamSpec::drop_lit` (+ `lit_drop_value`, `folds_to_literal`, the `$lit` instance
        names, `subst::assigns_to`), `ParamSpec::drop_none` (+ `is_unpinned_optional`, `$dn`), the pair-of-literals
        refit, and `TExpr::defaulted` (a defaulted node is stamped like any other: an unconstrained `none`, `[]` or
        numeral IS an `i64`-based value). Consequences accepted as the uniform rule: `wrap(5)` and `wrap(7)` are one
        instance (was one per value); `g([])` is `g$i64` (was `g$EmptyArray`); `identity(5)` is a call (was folded).
        The probe's form names were wrong for numerals (`Num` vs `Int`), so no numeral had ever been stamped from the
        pinner; fixed. Anonymous `any` type variables (`any$N`) are the pinner's own names and are not stamped.
        Mono's tests now run through `frontend`, since mono's input is a program whose nodes say what they are.
        Left (stage 4): `NoneInner`/`EmptyArrayArg`/`NoneLiteralArg`/`pseudo_marker` are now unreachable from stamped
        input and can be deleted with the checker's stamping.
- [~] **3. Passes read types instead of guessing them.** Revised in Session 8: porting every pass to `TExpr` is a
      type-safety nicety, not what stops the optimizations. What stops them is that they cannot ask a node's type,
      and since stage 2 every node carries `Expr::ty` (pre-mono, from the pinner; post-mono, from `infer`). So the
      work is: make each pass read the stamp where it guessed, keep stamps right where a pass builds nodes, and
      only then (stage 4) make the type mandatory by switching representation. After each step `--- report ---`
      and baseline rows must not move.
  - [x] `fold_extends`: `ArrayReceivers`' per-function declaration census (`bound_names`, the once-bound rule,
        `of_function`) is gone; a source is an array when the pinner stamped it one. The *receiver* rule stays
        syntactic on purpose (annotated, or seeded by `[]`/a reservation): a receiver that merely holds another
        function's array measured worse when pushed into (`optimizations/fold_extends_value_or`).
  - [x] Post-mono nodes are typed: `Mono::infer` stamps every node it returns with its inferred type (not unit,
        not a placeholder or `ConcatStr`), so passes over the `MonoProgram` can read `e.ty`. Nothing reads them
        yet.
  - [ ] Passes that still guess or are syntactic about types: see the audit list in the notes (Session 8).
  - [ ] Keep stamps right where a pass builds or moves nodes (the stale stamps found in Session 7).
- [~] **4. Retire the old.** The pinner becomes the checker; `check.rs` goes.
  - [x] **4a. The checker no longer rewrites the program** (Session 12). `frontend` runs `check_resolved` for its
        findings and the name resolutions only (it now returns just those), and everything downstream reads the
        *lowered* program typed by the pinner: stamps, plus what `stamp_locks` used to do, now in
        `probe::stamp_everything` (written `f<T>(x)` type arguments stripped; a value lambda's inferred parameter
        types written onto it; `int_parse` retargeted to its stamped integer type). Deleted from the checker: the
        `check` entry point, `stamp_locks`, `retargeted_int_parse`, `lambda_params`. No baseline row moved.
  - [ ] **4b. Findings from the pinner.** Measured (Session 12): with the pinner's rejections reported instead of the
        checker's, 189 of 270 `err_*` cases still match byte for byte and 81 differ: different spans (`true + false`
        underlined whole vs the operand), different wording (`both sides must be the same type` vs `the operands are
        bool and i64`), and fewer findings per function.
        - [x] **Recovery** (Session 13). The pinner goes on past a rejected statement the way the checker does, at the
          same positions and no others (`Pinner::recover`): a discarded statement (`Seq`'s first), an assignment, and
          the elements of an array or set literal. A `let` initializer is deliberately not one (the body is typed
          against what it produced), see `err_one_per_binding`. A failed statement leaves a unit node of an unknown
          type; once any error is held the body is not settled or finished, so nothing downstream reports its echoes.
          `Pinned::failures` holds one entry per error. `oracle_report` now compares the two lists on the checker's
          rejected programs (`tests/cases`: 128): the same errors in 58, at the same places in 94, others in 34.
        - [ ] The 34 whose places differ are the findings the pinner lacks: `without []` argument checks, a missing
          check per form (`cannot index/slice a value of type`, `for-loop iterable`, variadic `without []`,
          type-parameter bound errors `requires "int"`, `narrow` without a target, method-signature rules on
          declarations, `cannot return a function value`, closure captures/untyped parameters, shim closure rules),
          and spans the checker puts on the operand/annotation where the pinner uses the literal (`300` as `u8`).
        - [ ] Match checks (`match_check.rs`), lints that read checker state (`locks`: `narrow_target_errors`,
          `return_pinned_errors`), then refill the sections.
  - [ ] **4c. Name resolutions from the pinner** (`aipl_syntax::resolution`, read by the LSP and index), after which
        `Cx` and its ~6,000 lines go and the helpers the pinner shares (`coerce`, `validate_type`, `MatchInfo`,
        `generic_instance_name`, ...) move to a module of their own.
  - [ ] **4d. Delete the pseudo-types** (`NoneInner`, `EmptyArrayArg`, `NoneLiteralArg`, `pseudo_marker`) and make
        `Expr::ty` mandatory by switching the passes to `TExpr`. Behaviour-changing wins on exact types
        (`fold_extends`' receiver rule, the `tuple_windows` optional carry) are follow-ups with their own measurements.

## Risks

- A *silent* behaviour change: a rule the old checker enforced and the pinner forgets. Guards: the accept/reject
  oracle over the whole corpus and every existing `err_*` case still failing to compile.
- Defaulting everything unresolved to `i64` means a typo that leaves a variable unconstrained compiles instead of
  erroring; ambiguous *generic* variables still error.
- Size: ~2,000 `ExprKind::` sites across `aipl-mono`.

## Status / notes

- Session 1: stage 0 done. Stage 1 started: `pin.rs` (~2,200 lines) and `tests/suites/pin.rs` (the oracle report,
  `#[ignore]`d author helper); `aipl_codegen::lowered_for_check` is `frontend`'s first half, exposed so the pinner
  can be run on programs the checker rejects.
- Session 2: burned down the accept direction. Whole corpus, per program (read as a ranking): 62,128 function
  bodies pin, 13 gap (a non-argument closure, calls through function-typed fields, `to_set` on a struct tuple),
  23 are rejected though the checker accepts (mostly `err_*` cases the pinner is rightly stricter on; the rest
  are int-width cases such as `takes_u64(x)` where a narrowed value is passed, and `i64[][]` vs `i64[,]`).
  Forms now typed: patterns of every kind, tuple literals (`TExprKind::TupleLit`, lowered by mono), `shim`,
  diverging branches (`aipl_syntax::diverges`), annotated closure parameters, tuple fields/`to_set`/`to_array`.
- Session 3: ported the diagnostics and closed the reject direction. Whole corpus (cases and the compiler's own
  sources): 62,156 function bodies pin, 0 gaps, 1 false rejection (fixed after the run: an index literal was forced
  to `u64`), and of the 128 programs the checker rejects the pinner rejects all 128. Ported: effects and shims
  (coverage, signature, undeclared), `main`/`!random`, integer literal range, unknown names/types/structs, type
  parameter bounds/defaults/ambiguity/explicit type arguments, generic struct inference, `ord`/`int`/`variant`
  bounds, statement-vs-expression `match`/`if`/`if let`/`shim` (`no_outer_assign`), discarded results and
  mutating methods, refinements (`without []`, `without C`, `collect_arms`/`as_case`), float operator rules,
  mixed-width arithmetic, ordered/dict/set element validity (via `check::validate_type`), tuple-pattern misuse,
  wildcard rules, exhaustiveness and reachability (shared with the checker through `match_check.rs`).
  Shared with the checker rather than re-derived (move them into the pinner at stage 4 when `check.rs` goes):
  `check::{validate_type, DeclTables, undeclared_effect_message, no_outer_assign, check_result_inspected,
  reject_float_operand, optional_map_signature, span_map_signature, defaults_only_on_functions}` and
  `match_check.rs` (the match rules, written against a `MatchInfo` trait both implement).
  The oracle is now a test: `pin::the_pinner_agrees_with_the_checker` runs over `tests/cases` (18s) and fails on any
  gap, false rejection (a program whose file records `--- check --compile ---` is allowed to be rejected) or
  miss; `pin::oracle_report` (ignored) also covers `crates/` (170s) and prints the ranking.
- Session 4: finished stage 1. `.test`/`.benchmark` bodies, parameter/field/payload defaults and `Never` are
  pinned. Pinning test bodies exposed pinner gaps that only test code reached, now fixed: a `T+` parameter given an
  array literal, grids (`grid[cell]`, `is_in_bounds_of`, `try_set`, `any` over a grid), `#>{..a, x}` keeping its
  order, a constructor name beside a `Case<V>` in `==`, an element of an array literal shown to meet its slot's
  refinement, an empty `#{}` used as a sequence, and `if let` with a nested tuple pattern. Whole corpus: 0 gaps,
  0 false rejections, 128/128 rejected. `pin::the_pinner_types_tests_defaults_and_diverging_statements` pins
  each new part directly. Next: stage 2 (mono over the pinned IR), on the user's go-ahead.
- Stage 1 is done when the oracle prints zero gaps, zero false rejections and an empty "accepts what the checker
  rejects" list over the whole corpus. It does, and `pin::the_pinner_agrees_with_the_checker` asserts it.
- Session 5: stage 2 started with the probe. Over `tests/cases` (618 programs that monomorphize, 2,349 instances)
  mono's inferred type equals the pinned one at 12,690 of 13,306 compared nodes (95.4%); 1,815 nodes have no
  pinned counterpart (introduced by mono) and 296 hold a placeholder in mono's type. The 616 disagreements are
  mostly mono being coarser: an integer literal typed `i64` where the pinner knows `u64` (143), `u8` (40), `i16`,
  `i8`, `u32` (the widening `infer` leaves to later passes, which is what the project removes); a mutating call's
  desugaring typed `i64`/`()` where the pinner has the statement's type (118 + 16); and nodes a desugaring gives the
  span of what it replaced, so the probe cannot tell them apart (`ones`, `set_field_nested`). Not yet sorted into
  "mono wrong", "pinner wrong" and "span collision"; do that before 2b. `pin::mono_agreement_report` prints the
  ranking (`#[ignore]`, 18s).
- Session 5 (continued): sorted the probe's disagreements and fixed the pinner's side of them: a function that
  returns the sequence it was given returns a `str` as a `str` (the signature sees `char[]`; the pinned node did
  too), and a constructor name where a `Case<V>` is wanted (payload-carrying wrapper or nullary) has type
  `Case<V>`, not the constructor function or the variant. `Probe::finish` now sets aside disagreements at a span
  where mono itself gave several types (146). What remains is mono being coarser than the pinner: integer
  literals typed `i64` (about 225), a `print`/`put` statement typed `i64` where it is `()` (about 117), `!x`
  typed `()` (16), `int_parse` targets (5); plus nodes a desugaring gives another node's span.
- Stamps (`pin::stamp_agreement_report`, `#[ignore]`, tests/cases plus crates): of 122,260 stamped expressions
  (nodes keyed by span *and* form, which removed the shared-span noise) the pinner's type equals the checker's
  stamp at 122,257. The three others: `err("..")` stamped `()!Error` where the pinner has `()!str` (the pinner
  binds `E` from the argument before the function's `!Error` is in view), and two `map` calls over a `str` that the
  checker stamps `char[][]` and the pinner types `str[]`. The checker also rewrites `int_parse` targets while
  stamping (`retargeted_int_parse`); that is not a type and stays with whatever replaces it.
- Decision (Session 6): stamp everything. `frontend` now stamps every node the pinner typed (`probe::stamp_everything`,
  the two trees walked together; span and form only as a fallback), except unit and never. The experiment that led
  to it, run behind a switch: Over `cases_arrays`, `cases_generics` and `cases_optimizations` 171 of 188 cases still pass and
  17 fail: 16 on a moved baseline row (between -16% and +3% code bytes, mono now seeing exact types where it
  guessed) and `arrays/push/err_push_char_from_empty`, which stops being an error. So blanket stamping is the
  behaviour change that stage 3/4 is for, not a free swap now; the exact set of nodes the checker stamps has to be
  reproduced first, or the moved rows accepted as their own measured change.
- Session 7: stage 2c begun. The oracle also pins the synthesized `__test$` programs (`check`'s own driver
  programs). Literal widths now come from the stamp (see 2c above); the blanket-stamp experiment is recorded there.
- Session 8: stage 3 reframed (see above). `fold_extends` reads stamps (`known_array`), `Mono::infer` stamps its
  output. Baseline rows did not move across arrays/lambdas/generics/optimizations/strings/loops.
- Session 8 audit of passes that still work around missing types (each is a candidate for the next step, and a
  behaviour-changing win with its own measurement): `fold_lengths::mentions_number` skips a literal array whose
  elements mention a number because their widths were unknown (the stamps now say); `fold_lengths::fold_index`
  folds an out-of-range index only into an *empty* literal because only its recorded type could type the `none`
  (every literal has a stamp now); `fuse/loop_fusions` peels the first element of a `tuple_windows` loop to avoid
  annotating `mut $prev: T? = none` (the element type is now readable); `counting_variants::is_array` inspects
  declared return types. Not type-related: `known_constructor_elimination` (matches on constructor shape).
- Session 9: the audit's behaviour-changing wins, measured. (1) `fold_lengths::mentions_number` became
  `mentions_untyped_number`: a numeral that carries a stamp keeps its width when lifted out of its array, so the
  index fold no longer refuses elements with numbers. (2) `fold_index` now folds an out-of-range *numeric* index of a
  non-empty literal into a `none` typed from the literal's stamp (`none_for`). Together: `optimizations/fold_emptiness_and_index`
  -4.6% instructions, -4.5% code bytes, one fewer allocation; nothing else in arrays/lambdas/generics/strings/
  optimizations moved. (3) NOT a win, reverted: carrying `mut $prev: T? = none` in the `tuple_windows` fusion instead
  of peeling the first element is +33% instructions on `loop_tuple_windows_fusion` (24,901 -> 33,243) for 200 fewer
  code bytes; the peel stays and its comment says why. `counting_variants::is_array` reads declared return types, not
  a guess about a value, so it is not a candidate.
- Session 10: re-pin after the passes + one stamp rule in `infer` (2c done). Stale stamps were never the pinner's
  fault: re-pinning the optimized program makes every stamp describe the tree mono reads, at the cost of one more
  pinner run per compile. Preference recorded: Drew prefers regular structure and architecture to performance, so a
  uniform rule that costs a few instructions beats a special case that saves them (this also reopens the
  `tuple_windows` optional-carry loop, +33% instructions on its case, as a simplification candidate).
- Session 10 (continued): the first handoff of the re-pin failed twice, both worth knowing. (1) `ConcatStr` is a
  representation `infer` gives a `str` argument that the stamp (a plain `str`) cannot say, so an inferred `ConcatStr`
  is never replaced by a stamp. (2) A pass can remove the context that decided a type: `inline_single_use` puts a
  callee's body into its caller, and the body's `to_set()` was only an ordered set because the callee's return type
  said so; re-pinning then typed it unordered. The pinner now meets a node's existing stamp (`Expr::ty`) with the
  type it finds (`Pinner::expr_at`): a stamp is the recorded context, and it survives the passes that move a node.
- Session 11: 2d. See the 2d entry. Baseline rows moved by the uniform rule (instances shared per type, defaulted
  `none`/`[]` typed `i64`); reviewed in the handoff diff.
- Session 11 (continued): the first handoff of 2d died in one `aipl check` shard: `variant "Rule$i64" case "Term" is
  still abstract: Case<i64>`. A generic application whose argument nothing decides (`Rule<K>` in
  `Then([..]).show()`) must not be defaulted to `i64`: `K: variant` is not satisfied by it, and monomorphization settles
  it from the instances the program has (`sole_instance`). Nodes whose type holds such an application carry
  `TExpr::unsettled` and are not stamped. That is the one place a defaulted type still stays out of the stamps; moving
  the `sole_instance` rule into the pinner (a program-level default for a bounded variable) would remove it.
- Session 12: stage 4a. The checker is validate-only now. 4b/4c are the remaining way to delete it; 4b has a
  measured shopping list (above).
- Session 12 (continued): the first handoff of 4a found two things the corpus oracle could not. (1) The pinner
  rejected `projects/minesweeper/main.aipl` (`?` propagating a `str` into `!Error`: the error sides are now met with
  `agree`, which knows `Error` is a `str`), and the pinner-agreement test only covered `tests/cases`; it now covers
  `examples/`, `projects/` and `tests/ffi_fixtures/` too. With the checker no longer stamping, a function the pinner
  rejects has no stamps at all, so a pinner gap is now a compile error downstream, not a quiet loss of precision.
  (2) `Refuse(Msg("no")).show()` needs `K` from the program's one `Rule<K>` instance: the checker's rule (an
  argument, then the return type, then the sole existing instance) now also lives in the pinner
  (`Pinner::settle_open_applications`), counting the declared instances, those in functions pinned before, and those
  in the function itself, so the mono-side `sole_instance` fallback no longer decides what a stamped node is.
- Session 13: 4b recovery. The pinner reports every error in a body at the checker's own recovery positions
  (discarded statements, assignments, array/set literal elements); `Failure`s are one per error. No behaviour change:
  nothing reports the pinner's errors yet, so the gate is unaffected. The oracle's new comparison is the shopping list
  for the remaining findings.
