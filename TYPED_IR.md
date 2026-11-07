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
- A `Return` expression has no value; the pinner gives it a fresh variable (it unifies with anything, then
  defaults). A `Never` type would be cleaner and is a follow-up.

## Running the oracle

`cargo test --test compiler -- --ignored pin::oracle_report --nocapture` loads every `.aipl` under `tests/cases/`
and `crates/`, runs the checker's `frontend`, pins what it accepted, and prints pinned / gap / rejected counts
with the commonest gaps and rejections, each with an example file and function. `AIPL_CASE=a,b` narrows it (a
path substring list; a few directories take seconds, the whole corpus about three minutes). Counts are per
program, and every program carries the builtins and any compiler sources it imports, so the same function is
counted many times: read the *ranking*, not the totals. A "rejected-but-checker-accepted" line is a missing
coercion or special case; a "gap" is a form not yet typed.

Not measured yet: the other direction (programs the checker rejects must be rejected by the pinner). `frontend`
returns `Err` for those, so there is no lowered program to hand the pinner; it needs `frontend` split so the
lowering half (everything before `check_resolved`) is callable on its own.

## Stages (each ends green: targeted tests, then one `cargo handoff`)

- [x] **0. `typed.rs`**: `Ty`, `ConcreteTy`, `TExpr`, `TProgram`, `erase`, `concretize`; 6 unit tests. Additive.
- [ ] **1. `pin.rs` as an oracle, beside the old checker.** Nothing consumes it. (In progress; see Status.)
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
- [ ] **2. Monomorphize over pinned IR.** Old mono selectable (`AIPL_PIPELINE=old|typed`, test only) until the
      new `MonoProgram`, erased, equals the old one for the whole corpus and the dogfood IR fingerprint is
      unchanged. Then delete old `infer`.
- [ ] **3. Port the passes**: pre-mono first, then post-mono. After each, `--- report ---` and baseline rows must
      not move.
- [ ] **4. Retire the old.** Delete `check.rs`, `ArrayReceivers`/`holds_array`/`known_array`,
      `narrows_literal_array`, `is_unpinned_optional`, `Expr::ty` stamping; the pinner becomes the checker and
      the diagnostics sections are refilled. Behaviour-changing wins (fold_extends on exact types, ...) are
      follow-ups with their own measurements.

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
- **Reject direction (the remaining stage-1 work).** Of the 128 programs the checker rejects, the pinner rejects
  54 and *accepts 74*. They are the diagnostics not ported yet; the oracle prints the list under "the pinner
  accepts what the checker rejects". By family: effects (undeclared, shim coverage/signature, `random`), `ord`
  and int bounds, float operand rules, integer literal range and mixed widths, exhaustiveness/unreachable/
  wildcard rules for `match`, `no_outer_assign` (an expression `if`/`match`/`if let`/`shim` that mutates),
  statement/expression `match` use, unused results, set/dict element validity, refinements (`err_nonempty`),
  unknown types in signatures, unimported builtins (several are hidden behind a gap), explicit type args and type
  parameter defaults, generic inference failures (`err_generic_cannot_infer`, `err_return_only_type_param`).
  Port each from `check.rs` (search the message text of the matching `err_*` case's `--- check --compile ---`).
- Not pinned yet: `.test` bodies (`TFunction::test_body` is `None`), parameter defaults and struct field defaults
  (`default: None`), and `Return` has a fresh-variable type rather than a `Never`.
- Stage 1 is done when the oracle prints zero gaps, zero false rejections and an empty "accepts what the checker
  rejects" list over the whole corpus, then it becomes a real assertion (drop the `#[ignore]`).
