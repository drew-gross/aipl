# Plan: make the type on every expression mandatory (`TExpr` end to end)

Status: step 1's probe is run (below); the port has not started. Written after the pseudo-types were
deleted (`Remove pseudo-types`).
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

## Probe results (the first action, done)

`cargo test --test compiler -- --ignored pin::mono_agreement_report --nocapture` (about 20s) runs the
pinner and mono's `infer` side by side over `tests/cases` and `crates`, unoptimized, comparing each
node by function, span and form. Over 756 programs (214 are the `err_*` cases, which do not
monomorphize), 5,426 instances:

- 75,201 nodes compared, 74,979 agree (99.7%). 222 disagree, in four groups:
  - About 190 are statement-position nodes (`print`, `put`, `Assign`, `Match`, a shim, a call to a
    void function): `infer` gives them `i64` or a slot array where the pinner says `()`. Not a type
    disagreement; `infer` has no unit for "no value" and the stamp-first rule hid it.
  - 20 `Let` nodes: `infer` says `()`, the pinner says the body's type.
  - 17 `extend_map` / `extend_filter` calls: `infer` says `()`, the pinner says the array being
    extended (the value of a mutating call).
  - 2 `int_parse` calls (`i8?`, `i32?` pinned, `i64?` from `infer`): the pinner is right, `infer`
    defaults the width and a stamp covered it. This is the only kind of disagreement where `infer` is
    simply wrong.
- 787 nodes are not compared (`unbound`): the probe derives an instance's type arguments from its
  parameter types, and cannot when a type variable is not in any parameter. They are the generic builtin
  bodies (`drop_first`, `tuple_windows`, `repeat`, `grid_get`) and the `unsettled` `Rule<K>` /
  `StepResult<K>` / `Token<K>` family in `format.aipl` and `lex_aipl.aipl` (hard part 4). The port
  does not need the probe's guess there: it reads `type_args` off the call. But the probe cannot
  vouch for these until it does too.
- 3,804 nodes are `unmatched`: synthesized by a pass or by mono itself, so they have no pinned
  counterpart. That is the hand-typed set (hard part 2) that the port must type itself.

Reading: the two derivations agree wherever both are defined, so step 1 is a day or two of porting,
not three. The cost is the 608 match sites, the 3,804 synthesized nodes, and a settle-from-instances
step for `Rule<K>`; the disagreement tail is small.

## Order revised: step 2 first, behind an adapter at mono's output

Reading mono: `infer_node` is a ~1,500-line fusion of typing and rewriting, and the pinner's types
already feed it as the expected type (`Expr::ty`, stamp-first). A faithful `Expr` -> `TExpr` port of
mono has no green state in between. The claim above that an adapter "would need the types that were
lost" is false at mono's output, where `infer` stamps every non-unit node it returns. So:

1. An adapter at the end of `lower_program` turns the stamped `Expr` bodies into
   `TExpr<ConcreteTy>`, failing on any node that is neither typed nor structurally unit.
2. The post-mono passes and codegen move to `TExpr<ConcreteTy>` (about 350 sites), and codegen
   reads `expr.ty`: `has_placeholder_ty`, `stamped_ty` and the `i64` fallback go.
3. Mono's inside and the pre-mono passes port afterwards, deleting the adapter as they go.

### Coverage of the stamps (measured)

`cargo test --test compiler -- --ignored pin::typed_coverage_report --nocapture` (about 10 minutes: it
runs the full optimizer on every program). Over 829 programs, 5,689,355 nodes after
monomorphization and the post-mono passes:

- 4,745,762 (83%) carry a type, and none carries a type a `ConcreteTy` cannot say (no concat string
  or placeholder leaks through the stamps).
- 943,593 carry none. By form: `Unit` 331k, `Let` 217k, `Assign` 111k, `Seq` 76k, `Return` 31k and
  `LetMut` 1.5k are structural (a unit, or the type of the body or last statement) and can be
  computed from their children. The ones that are lost types: `Call __move` 73k (built by
  `move_last_use` around a value), `If` 49k, `Ident` 34k, `Match` 16k, and a tail of `Call
  __builtin_equal` 706, `Num` 461, `__filter_*` 1.2k, `Str` 245, `Field` 50, the `extend_map`
  instances of `subexprs`, shim cell calls and the terminal builtins.
- Done so far, with the coverage test now counting a node as *derivable* when its form says what it is
  from its children (a `let` is its body, an `if` its branch, an assignment unit) and as *lost* only
  when nothing does:
  - `move_last_use` builds `__move` with `moved(inner)`, which keeps the operand's type.
  - `stamp_idents` runs last in `lower_program`: it types each bare name from its binder (a parameter,
    a `let`, a `for`, a match arm's pattern read against the scrutinee's type), a comparison as `bool`
    (a literal operand takes the other side's type), a string literal as `str`, a `Field` from the
    struct declaration, and a `__move` as its operand.
  - On a spot run (209 programs, 13,343 nodes) the lost nodes went from 87 to 9, all calls to
    unit-valued intrinsics (`__filter_drop`, `__filter_truncate`, `__map_set`, the mutating
    `extend_*` instances). The adapter has to say those are unit by callee.
  - `If` and `Match` were never lost: an untyped one is a statement whose branches are unit.
- The adapter (`aipl-mono/src/typed_adapter.rs`): `on_typed(program, pass)` types each body
  (a bare name from its binder, a statement form as unit, a binding or sequence as its body, an
  `if`/`match` as a branch, a call to a unit-answering builtin/instance as unit, an unfitted
  literal as `i64`), hands it to `pass` as `TExpr<ConcreteTy>`, and converts back. Conversion adds
  nothing: a node that was not stamped (`TExpr::derived`) comes back unstamped, and a binding that
  was annotated (`TExpr::annotated`) keeps its annotation, which is its value's type. A concat
  string, which `ConcreteTy` cannot say, travels as a named type of its own.
- Passes on it (each through `on_typed`/`on_typed_where`/`on_typed_named`): `fuse_try_with`,
  `fold_matches`, `sink_bindings`, `eliminate_known_constructors`, `move_last_uses` (all
  `*_post_mono`), and the expansion step of the two post-mono inliners (`inline_typed.rs`: the
  decisions are still made on `Expr`, the bodies a call sits in and the body it expands to are
  converted, spliced, and converted back). Helpers they share are in `typed_util.rs` (children,
  `mentions_free`, `rename_scoped`, `uses_of`, ...), `early_exit.rs` has a typed twin.
- A binding's `annotation` lives on the `let` node (`TExpr::annotation`) and can differ from its
  value's `ty`: the inliner binds a `char[]` argument as the parameter's `str`.
- Twins still written twice, until mono and the pre-mono passes are typed: `fuse_try_with`,
  `fold_match`, `sink_here`, `early_exit`, the analysis half of `move_last_use` (mono's early run
  still calls the `Expr` one).
- Found on the way: mono's `node(..)` stamped the call's type on the `Field` of a call through a
  function-valued field (fixed: the field is left unstamped), and `children_mut` skips an
  assignment's target.
- Next: convert once before the post-mono pipeline instead of per pass (each pass pays a round
  trip now); then codegen reading `expr.ty`; then mono and the pre-mono passes, deleting the twins.

## First action when this is picked up (done; kept for the record)

Write the side-by-side probe (pinner type vs `infer` type per node, over `tests/cases` and `crates/**/*.aipl`)
and read the disagreement list. Its size decides whether step 1 is a day or three.

## Related, not part of this

- `fold_extends` receiver rule and the `tuple_windows` optional carry (CLAUDE_TODO.txt).
- The stale `__none__` comment sweep (CLAUDE_TODO.txt).
