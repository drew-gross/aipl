# Cases as types

Goal: a case behaves like any other type wherever the language takes a type — a pattern, a
type argument, a type parameter's range — so `as_case`/`is_case`/`collect_arms` can later take
their case as a type argument (`xs.collect_arms::<Circle>()`) instead of the constructor
special case they have now. That migration is **not** part of this plan; this plan makes it
expressible.

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
  `generics/explicit_type_args.aipl`), but never name a case: a `<X: cases>` parameter is a set of
  *excluded* cases, only declarable as `T without X` in the signature, and `case_params` is kept
  apart from `type_vars`, so a call's type-argument list does not count it
  (`fn "holds" takes 1 type argument(s) (T), got 2`).
- **There is no type parameter that ranges over one case of a variant**, so no function accepts
  "one case of any variant". `as_case`/`is_case`/`collect_arms` take `arm: Case<T>` and the
  checker special-cases that argument as a constructor name (`refine_collected_arms`, the
  "needs the case written as a constructor name" refusal pinned by `err_as_case_value.aipl`).
- `Case<V>` (the tag as a value, `Type::Case`) is a different thing from the arm type and stays.

## Committed decisions

- Arm types stay refinements of their variant; nothing about their run-time form changes.
- A case-kinded type parameter is a *type* parameter: it is passed and inferred like `T`, and
  an explicit call may name it. `cases` (the excluded set, used with `without`) is untouched.
- The new bound is spelled `<V: variant, A: arm of V>`: it names the variant, so a signature can
  relate `A` to `V` (`(xs: V[]) -> A[]`).
- A case with named payload is struct-like once its case is known: a value typed as the arm type has
  the payload slots as fields, and a struct-style pattern naming the case follows from that.
  A value of the plain variant has no such fields, as it has no `Circle(r)` pattern in a `let`.

## Open questions

- Whether `A` can be reflected to a `Case<V>` value in a body (needed to write `collect_arms`
  itself in AIPL: `self.filter(|x| x.is_case(<A as a value>))`). If not, the migration keeps a
  compiler-typed `collect_arms` and only retypes its signature.

## Stages

Each ends at a green `cargo handoff`. Stage 1 is independent of the others.

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

### 2. Type arguments that name a case

Goal: a normal function can take a case as an explicit type argument.

- [ ] A type argument written as a case name resolves to its arm type (`f::<Shape, Circle>`).
- [ ] A `cases`-bound parameter counts in the type-argument list instead of being kept apart.
- Watch: `case_domains`/`case_slots` in `aipl-mono/src/pin.rs` and the loader's explicit-argument
  arity check.

### 3. A type parameter ranging over one case of a variant

Goal: `fn f<V: variant, A: <new bound>>(x: A) -> ..` accepts any one case of any variant.

- [ ] The bound `A: arm of V`; inferred from an argument's arm type, or given by stage 2.
- [ ] Inside the body `A` is its variant (the arm type "goes wherever its variant does").
- [ ] Cases: inferred, explicit, and refused for a value that is not one case.
- Depends on 2.

## Done when

A function generic over one case of any variant can be written and called with the case
inferred or named (`::<Circle>`), `|Circle { r }| ..` and `c.r` work on a value known to be that case, and
nothing about `as_case`/`is_case`/`collect_arms` has changed.
