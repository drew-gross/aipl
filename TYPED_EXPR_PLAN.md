# Plan: make the type on every expression mandatory (`TExpr` end to end)

Status: not started. Written after the pseudo-types were deleted (`Remove pseudo-types`).
Delete this file when the work is done.

## Where things stand

- `crates/aipl-syntax/src/typed.rs` has `TExpr<T>`, `TExprKind<T>`, `TProgram`, `Ty` and `ConcreteTy`.
  `ty` is mandatory on every node; `Ty` can name a rigid `Var`, `ConcreteTy` cannot.
- `crates/aipl-mono/src/pin.rs` is the type checker and produces a `TProgram`.
- `frontend` (`aipl-codegen/src/lib.rs`, around `pinned_findings`) then calls
  `probe::stamp_everything`, which erases the typed tree back into `ast::Expr` and copies each type into
  the optional `Expr::ty`. Everything after that runs on `Expr`: the pre-mono passes, mono, the
  post-mono passes, and codegen's read of `ConcreteFn::body`.
- Mono's `infer` still derives a type for every node. A stamp overrides it except where the stamp is a
  placeholder or the node is `result_only`; `infer` writes its result back as the stamp. Codegen derives
  types bottom-up (`compile_expr_inner`) and reconciles with `expr.ty` (`has_placeholder_ty`,
  `stamped_ty`).
- So there are three derivations of the same fact (pinner, mono `infer`, codegen), and `Expr::ty` is
  `Option`, so a pass can still lose a type.

## Goal

One derivation: the pinner. Every later stage takes `TExpr` and cannot construct a node without a type.
Mono becomes substitution over pinned IR; codegen reads the type off the node.

## Size (grep counts, not a prototype)

| Layer | `ExprKind::` sites | node constructions |
|---|---|---|
| `aipl-mono/src/lib.rs` (mono) | 608 | ~240 |
| pre-mono passes (`known_constructor_elimination`, `fold`, `fold_lengths`, `fold_extends`, `appending_variants`, `sink`, `counting_variants`, `early_exit`, `subst`, `grid_literals`, `fuse_try_with`, `hygiene`, `fuse`, `passes.rs`) | ~650 | ~190 |
| post-mono passes (`move_last_use`, `reuse_matched_values`, `shim_closures`, `lower_reductions`) | ~150 | ~35 |
| codegen (`aipl-codegen/src/lib.rs`) | 98 (+87 `.ty` reads) | 6 |

About 1,500 mechanical match-arm edits, plus the shared traversal helpers (`children`,
`children_mut`, `mentions_free`, `references_name`, `is_receiver`) made generic over `TExpr<T>`.

The pass manager threads one program type through every pass, so the pre-mono passes move together. An
adapter back from `Expr` to `TExpr` is not an option: it would need the types that were lost.

## Order (each step ends at its own green handoff)

### Step 1: mono over pinned IR, passes still on `Expr`
- The pre-mono passes run as today. Re-pin afterwards with `Stamping::Replace` (the mode already
  exists), so mono receives a `TProgram` whose types are current.
- Mono becomes `TExpr<Ty> -> TExpr<ConcreteTy>`: substitute `Var` from the call's `type_args`, lambda
  lifting, `ParamSpec` specializations, `mutating` desugar, `lower_for`, `IfLet` to `Match`.
- Delete `infer`'s type derivation and the stamp write-back.
- Biggest single win; leaves the fifteen pass files alone. Probe first: run the pinner and old `infer`
  side by side over the corpus and list every node where they disagree, before deleting anything.

### Step 2: post-mono passes and codegen
- Input is already concrete, so no generics to substitute.
- `ConcreteFn::body: TExpr<ConcreteTy>`; codegen reads `expr.ty`.
- Delete the bottom-up reconciliation: `has_placeholder_ty`, `stamped_ty`, the stamp-first branches for
  empty literals, and the `i64` fallback for unstamped nodes.

### Step 3: pre-mono passes, one at a time
- Port each against its own targeted test and its `--- report ---` / baseline rows (no row may move).
  Suggested order, smallest first: `fuse_try_with`, `early_exit`, `grid_literals`, `counting_variants`,
  `appending_variants`, `fold_lengths`, `fold`, `sink`, `fold_extends`, `known_constructor_elimination`,
  then `subst`/`hygiene`/inlining last, since they carry the generic-substitution question below.
- Once the last one lands, delete `stamp_everything`, `Expr::ty`'s role as a checker output, and the
  re-pin from step 1.

Steps 1 and 2 are independent of step 3; stopping after either leaves a coherent tree.

## Hard parts to settle before or during

1. **Inlining a generic body.** The pinner typed the callee against rigid `T`; each inlined copy must
   substitute `T := concrete` through every node's `ty`. Needs a `TExpr` type-substitution walk driven by
   `type_args`. Main design cost of step 3.
2. **New nodes in passes.** Fusion builds loop bodies, accumulators, `none` arms, tuple accumulators.
   Most copy the type of the node they replace; about 100 are genuinely new and must compute theirs. The
   hand-stamped nodes in mono (`out_init`, zip's `mut $out = []`, `opt_variadic_seq`, struct-field
   defaults) are the list of the kind.
3. **Representation choices are not types.** `ConcatStr` and the `ParamSpec` flags (`owned`, `str_kept`,
   `set_kept`, `concat`, `variadic`, `drop_none`) need a home that is not `ConcreteTy`: a per-parameter
   flag, as `typed.rs` already commits to.
4. **`unsettled` applications.** The pinner flags `Rule<K>` with `K` defaulted so mono can settle it from
   the program's instances. Mono needs a real "settle from instances" step in place of `infer`.
5. **Pinner defaults vs mono inference.** Unresolved variables default to `i64` in the pinner; `infer`
   sometimes disagrees, and stamp-first hid it. Removing `infer` makes each disagreement a visible
   difference. Expect a tail of small bugs found by the corpus and baseline.
6. **Diagnostics now raised in `infer`** (ambiguous instance, missing impl) need a new home in the
   pinner or in substitution.

## What it buys

- Codegen's second derivation goes (the CLAUDE_TODO item "codegen re-derives types bottom-up").
- `fold_extends` can use exact receiver types and drop `ArrayReceivers`, `holds_array`, `known_array`
  (a behavior change: its own measurement, own handoff).
- A pass cannot drop a type by construction.
- No expected speed change; this is architecture.

## Risk

- Silent pinner/`infer` disagreement (item 5). Last round's 181 moved baseline rows show how sensitive the
  corpus is.
- Step 3 touches the passes that carry the most `--- report ---` and baseline claims: moved rows are
  likely there and must be explained, not refreshed blindly.
- Estimate to trust least: step 1, because it depends on how many disagreements exist, which only the
  probe reveals. Rough total: 2 to 4 days.

## First action when this is picked up

Write the side-by-side probe (pinner type vs `infer` type per node, over `tests/cases` and `crates/**/*.aipl`)
and read the disagreement list. Its size decides whether step 1 is a day or three.

## Related, not part of this

- `fold_extends` receiver rule and the `tuple_windows` optional carry (CLAUDE_TODO.txt).
- The stale `__none__` comment sweep (CLAUDE_TODO.txt).
