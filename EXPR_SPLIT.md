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
- `descendants_of_kind::<A>()` is spelled with the case as a type argument, like `collect_arms`.

## Open questions

- Whether `Expr { ..self, kind: other }` (a spread that changes the kind's type) type-checks for a
  generic struct; it is used throughout the desugaring.

## Stages

Each ends at a green `cargo handoff`.

### 4. Move the lints

Goal: the lints that start a `descendants()$f()` with `self.kind.as_case::<C>()?` use
`descendants_of_kind::<C>()` and destructure the kind in the parameter.

- [ ] One lint at a time (each is its own `.aipl` file with its own tests); the hit spans and
  messages do not change.
- [ ] `collect_expr_cases` (kinds alone) goes once nothing wants kinds without their node.

## Done when

`descendants_of_kind::<A>()` returns `Expr<A>[]`, and the lints that took a node's kind apart by
hand no longer do.
