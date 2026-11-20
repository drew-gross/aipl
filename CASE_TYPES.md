# Cases as types

Goal: a case behaves like any other type wherever the language takes a type — a pattern, a
type argument, a type parameter's range — so `as_case`/`is_case`/`collect_arms` can later take
their case as a type argument (`xs.collect_arms::<Circle>()`) instead of the constructor
special case they have now. The language work comes first; the migration is the last stage.

## Context

What exists today (read from the code and probed with `aipl check`):

- **A case is already a type, by sugar.** `Circle` written where a type goes is the arm type
  `Shape without Square, Dot`: `arm_type_decls` in `aipl-syntax/src/lib.rs` declares one
  refinement `Circle@Shape` per case a program mentions, and `collect_arms(Circle)` types its
  elements as it. `tests/cases/variants/arm_types.aipl` is the tour. A value of it goes wherever
  its variant does, and `let`, `for`, parameters and lambda parameters take its payload with the
  case's tuple pattern (`|Circle(r)| r` over `xs.collect_arms(Circle)` checks).
- **A value typed as one case of a plain variant has that case's named payload slots as fields**
  (`c.r`, `Circle { r }` in a `let`, `for`, parameter or lambda parameter): `ExprKind::Field` in
  `aipl-mono/src/pin.rs` lowers it to a one-arm `match` (`case_fields.aipl`). A generic variant's
  cases do not have them yet.
- **A call's type arguments can be written** (`f::<T, U>(x)`, `x.f::<T>()`; case
  `generics/explicit_type_args.aipl`), all of them or none, and a case name is accepted as one: it
  is its arm type (`same::<Circle>(c)`, even for a `T: variant` parameter;
  `variants/case_type_arguments.aipl`). What a type argument cannot do is range over *which* case:
  a `<X: cases>` parameter is a set of *excluded* cases, only declarable as `T without X` in the
  signature, and `case_params` is kept apart from `type_vars`, so a call's type-argument list does
  not count it (`fn "holds" takes 1 type argument(s) (T), got 2`).
- **`<V: variant, A: arm of V>` is a type parameter that ranges over one case of a variant**: lowered
  at the FFI boundary into a refinement parameter standing for the other cases, with `A` written
  `V without A` in the signature; inferred from an argument's arm type or written as a type
  argument, which may leave `V` out (`variants/arm_parameter.aipl`).
- `as_case`/`is_case`/`collect_arms` still take `arm: Case<T>` and the checker special-cases that
  argument as a constructor name (`refine_collected_arms`, the "needs the case written as a
  constructor name" refusal pinned by `err_as_case_value.aipl`).
- `Case<V>` (the tag as a value, `Type::Case`) is a different thing from the arm type and stays.

## Committed decisions

- Arm types stay refinements of their variant; nothing about their run-time form changes.
- A case-kinded type parameter is a *type* parameter: it is passed and inferred like `T`, and
  an explicit call may name it. `cases` (the excluded set, used with `without`) is untouched.
- The new bound is spelled `<V: variant, A: arm of V>`: it names the variant, so a signature can
  relate `A` to `V` (`(xs: V[]) -> A[]`). An explicit type-argument list may leave out `V`, which
  `A` determines: `xs.collect_arms::<Circle>()`.
- A case with named payload is struct-like once its case is known: a value typed as the arm type has
  the payload slots as fields, and a struct-style pattern naming the case follows from that.
  A value of the plain variant has no such fields, as it has no `Circle(r)` pattern in a `let`.

## Open questions

- Whether `A` can be reflected to a `Case<V>` value in a body (needed to write `collect_arms`
  itself in AIPL: `self.filter(|x| x.is_case(<A as a value>))`). If not, the migration keeps a
  compiler-typed `collect_arms` and only retypes its signature.

## Stages


Each ends at a green `cargo handoff`. Stage 1 is independent of the others; stage 3 follows stage 2.

### 1. Fields of a case of a generic variant

Goal: `h.value` and `Has { value }` work for a case of a generic variant (`Has<i64>`), as they do
for a plain one.

- [ ] `case_field` in `aipl-mono/src/pin.rs` only handles an owner that is a plain named variant; an
  `It::Gen` owner needs the case's payload types with the variant's type arguments substituted, and
  the struct-pattern desugaring annotates its temporary with the bare case name, which a generic
  case refuses (`"Has@Opt" takes 1 type argument(s), but 0 given`).
- [ ] The refusal for a plain variant says only `field access on non-struct value of type Shape`;
  when a case of it declares the field, say the value has to be known to be that case.
- [ ] Cases for each under `tests/cases/variants/`.

### 2. Finishing the arm parameter

Goal: the rough edges of `<V: variant, A: arm of V>` (`variants/arm_parameter.aipl`).

- [ ] An arm parameter nothing infers (a result-only one with no `::<Case>`) stays unsettled, and the
  message names the parameter (`body returns (Shape without A)[] ..`) instead of saying it has to be
  written. Inferring it from where the result goes is not done: `agree` only settles a slot from the
  value given, not from the type wanted.
- [ ] The explicit-argument refusal says `one case of "V"` with the parameter's name; say the variant.
- [ ] A body cannot yet produce an `A` from a `V` (it is `V` less cases it cannot see), so
  `collect_arms` itself needs a compiler hook or a way to reflect `A` to a `Case<V>` value (see the
  open question); stage 3 decides.

### 3. Retype `is_case`, `as_case` and `collect_arms`

Goal: the three builtins take their case as a type argument — `x.is_case::<Circle>()`,
`x.as_case::<Circle>()`, `xs.collect_arms::<Circle>()` — and the compiler's constructor-name
special case for their `arm` argument is gone.

- [ ] Signatures use the stage 2 parameter; the `arm: Case<T>` parameter and `refine_collected_arms`'s
  reading of a constructor reference go, and the result types come from the type argument
  (`x.as_case::<Circle>()` is a `Circle?`, `xs.collect_arms::<Circle>()` a `Circle[]`).
- [ ] The "needs the case written as a constructor name" refusal and `err_as_case_value.aipl` go
  with it: a value in type-argument position is a type error like any other.
- [ ] Every call site in `crates/`, `tests/cases/`, `examples/`, `projects/` and the docs moves to
  the new spelling; the lints that quote the old form (`match_case_as_case`, `let_else_as_case`,
  `case_of_equal_is_case`, `return_loop_find_if`, `return_loop_any_all`, `return_loop_find_index`)
  advise the new one; the write-aipl skill and the docs say it.
- [ ] Decide whether the old spelling stays as an alias for one round or goes at once.
- Watch: this is the one stage that touches the whole corpus, so it is a fanout from the failure
  list; the `Case<V>` value type and `case_of` stay as they are.
- Depends on 2.

## Done when

A function generic over one case of any variant can be written and called with the case
inferred or named (`::<Circle>`), `|Circle { r }| ..` and `c.r` work on a value known to be that case, and
`as_case`/`is_case`/`collect_arms` take their case as a type argument.
