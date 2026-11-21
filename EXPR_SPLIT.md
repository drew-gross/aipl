# Descendants of one kind: `Expr<K>`

Goal: `body.descendants_of_kind::<Assign>()` returns `Expr<Assign>[]` — each matching node with
its `kind` typed as that case alone and its spans and source text beside it — so a lint writes
`let Expr { kind: Assign(target, value, _), span } = node;` with no `as_case::<Assign>()?` guard.
Most of the 26 lints that walk `body.descendants()` start by taking each node's kind apart that
way. `Expr` becomes generic over its kind: `Expr<ExprKind>` is today's `Expr`, and `Expr<A>` for an
arm type `A` of `ExprKind` is a node known to be that case.

## Context

- `Expr` (`crates/aipl-codegen/src/expr.aipl`) is a struct: `kind: ExprKind`, `span`, `value_span`,
  `name_span`, `source: str`, `written: WrittenNames?`. `Expr` is written 350 times in 46 `.aipl`
  files, and `ffi_ast.rs` decodes the struct by field name into `aipl_syntax::ast::Expr`.
- `descendants(self: Expr) -> Expr[]` is every node under `self`, outermost first.
  `collect_expr_cases(self, arm: Case<ExprKind>) -> ExprKind[]` returns the kinds of one case,
  typed as that case alone, but drops the spans.
- A function with a parameter `arm: Case<T>` is called as `f::<Case>(..)` or `xs.f::<Case>()`
  (the loader's `kwargs.rs` puts the case back as the argument at the parameter's position), and a
  result that is `T[]`, `T?`, `S<T>[]` or `S<T>?` is refined to that one case
  (`refine_collected_arms` in `aipl-mono/src/pin.rs`; `variants/case_parameter.aipl`). A concrete
  `S<T>` is an instance named for its arguments (`Tagged$Shape`), so the refined result is rebuilt
  as the application, read back from the name when every argument is the variant (one parameter, as
  `Expr<K>` has). The refinement inside an argument is not checked against the instance.
- A generic struct instance widens over a refined argument: `Node<Circle>` is accepted where
  `Node<Shape>` is wanted (probed). So an `Expr<A>` goes wherever an `Expr<ExprKind>` does.
- A struct's or variant's type parameter may have a default (`struct Expr<K = ExprKind>`, trailing
  ones only): `fill_type_defaults` in `aipl-syntax` writes out the arguments of a bare use or a
  partial application (`variants`/`generics/struct_type_parameter_defaults.aipl`), in every type
  position, after the loader drops the annotation a struct pattern's temporary carries. A bare name
  with a parameter that has no default is as it was.
- CLAUDE_TODO already lists `Expr<K>` as the way to say a rebuilt tree is clean of cases.

## Committed decisions

- `Expr<K = ExprKind>` with `kind: K`; a bare `Expr` means `Expr<ExprKind>`, so the existing code is unchanged.
- `descendants_of_kind(self: Expr, arm: Case<ExprKind>)` is in `expr.aipl`; `body.descendants_of_kind::<Assign>()` is an `Expr<Assign>[]`.
- Compilation is a sequence of progressive refinements, each stage's output typed as what it has established. `resolve_callees.aipl` turns a `Program` into a `ResolvedProgram` (`Program without Unresolved throughout`): every `Call` carries a `callee: Callee` that says what it is to, and the parser's `Unresolved` is gone. The lints take a `ResolvedProgram`, so one that skipped the pass does not type. `Callee` is the variant in `builtin_table.aipl`: an arm per importable builtin (as the Rust enum has them), `Some`/`Ok`/`Err`, `User(name)`, `Local(name)` (a local holding a function hides the name) and `Unresolved`. A lint matches `callee == Callee.Len`; it never compares an imported spelling. The advice a lint gives still spells a builtin as the file does (`filled`), and a lint of a call that is not yet ported may still use `bound_to`/`imported_as`.
- `descendants_of_kind::<A>()` is spelled with the case as a type argument, like `collect_arms`.

## Open questions

- Whether `Expr { ..self, kind: other }` (a spread that changes the kind's type) type-checks for a
  generic struct; it is used throughout the desugaring.

## Stages

Each ends at a green `cargo handoff`.

### 4. Move the lints

Goal: the lints that start a `descendants()$f()` with `self.kind.as_case::<C>()?` use
`descendants_of_kind::<C>()` and destructure the kind in the parameter.

- [ ] Moved: `field_init_shorthand`, `set_spread_field`, `value_or_compare`, `len_gt_zero`, `is_empty_longhand` (the last three share `call_lints`), `match_bool_is_case`, `match_is_some_and`, `match_value_or`, `match_value_or_err`, `match_map_value_or`, `match_map_err`, `match_map_ok` and `match_case_as_case` (they share `match_lints`; a lint about the function it is in, as `match_value_or_err` is, takes it beside the `Match`), `case_of_equal_is_case` (in `call_lints`, as an `Advice`), `match_case_try_as_case` (still over the function's tail matches, but reads `Some` by `callee`), `push_loop_pipeline` and `push_loop_collect_ok` (the two share `let_mut_lints`; a lint of a seed and its loop takes the `LetMut` and reads the loop through `plain_seeded_loop`, and the builtin that fills it is `Push`, not a name from the imports). `accumulation.aipl`'s helpers take the adding builtin as a `Callee`, so `extend_loop_map_join`, `push_loop_zip_with` and `push_array_literal` no longer read the file's imports for it. A lint over one kind is a function of one node (`self: Expr<C>`, destructured in place or read as `self.kind`) with no helper of its own unless it is shared; `lint.aipl` finds the nodes of a kind once per function (`construct_lints`, `assign_lints`) and maps each lint over them. A lint over a new kind adds a helper there. A lint of one node returns what it says without a span (`Advice`, `advice(builtin, before, after)`); `lint.aipl` places it at the node (`hit_at`), so the lint takes the case alone (`self: Call(..)`).
- [ ] One lint at a time (each is its own `.aipl` file with its own tests); the hit spans and
  messages do not change. A lint that matched imported names moves to `callee` matching as it moves.
- [ ] `collect_expr_cases` (kinds alone) goes once nothing wants kinds without their node.

## Done when

`descendants_of_kind::<A>()` returns `Expr<A>[]`, and the lints that took a node's kind apart by
hand no longer do.
