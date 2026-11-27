# Effects in function types

Goal: a function type says which effects calling it performs — `(A) !prints -> B` — a lambda or
function name of that type may perform them, and calling a value of the type needs the same effects
declared by the caller, exactly as calling a named function does.

## Context

- A function type is `Type::Fn(params, ret)` in the source AST (`aipl-syntax`), `Ty::Fn` once typed,
  `It::Fn` while the pinner infers, `ConcreteType::Fn` after monomorphization: about 126 matches in
  the Rust crates, and the dogfooded parser builds the same case (`TyFn` in `grammar_aipl.aipl`,
  `item.aipl`), so the AIPL AST and the Rust one have to change together.
- A function value performs no effect today: the pinner clears the effect set while typing a lambda
  used as a value (`value_arg`, the lambda arm of `expr`), and the checker refuses a named function
  with effects used as a value. Calling a value (`Call` with `Callee::Local`, a function-typed field)
  checks no effect, since none can occur.
- A lambda handed to a function that takes a function is specialized into the callee; the
  specialization's effects are those of the functions bound to its parameters
  (`specialization_effects`), so a higher-order function is effect-polymorphic without writing it.
  That stays.
- A named function declares effects after its parameters: `fn f(a: A) !prints -> B`; a call to one is
  refused unless the caller declares them (`undeclared_effect_message`).

## Decisions

- **Syntax:** `(A) !prints -> B`, `(A) !prints !reads_files -> B`; effects in a fixed (sorted) order
  and each at most once, like a signature's. A pure type is unchanged.
- **Subtyping:** fewer effects fits more. A function with effects `E` is accepted where a function
  type with effects `F ⊇ E` is expected; never the reverse. Parameter and return types stay
  invariant as they are now.
- **A lambda's effects** are the effects its body performs when it is checked against a known
  function type (the expected type's effects are what it may perform); with no expected type
  (`let f = |x| ..`) they are inferred from the body.
- **Calling a function value** requires its effects in the caller, for `f(x)`, `x.f()` and a
  function-typed field alike, with the message a named function gets.
- **A named function used as a value** has the type of its signature, effects included.
- **Closures keep working as variants:** the signature a closure variant is made for includes the
  effects, so `apply$n` declares the effects of its type and the calls inside it are well-formed.
- **Shims** need nothing new: a shim stands in for an operation at run time wherever it is called.

## Open questions

- Whether a type variable can stand for an effect set (`<E> (A) !E -> B`). *Recommended:* not yet;
  specialization already makes higher-order functions polymorphic over the effects they are handed.

## Stages

Order: 2 first; 3 and 4 need 2; 5 last.

Stage 1 is built: `Fn` carries `effects: Vec<String>` (sorted, each once, set by the reader in
`ffi_ast.rs`) in `ast::Type`, `ConcreteType`, `Ty`, `ConcreteTy` and `It`, and the AIPL AST
(`Fn(Ty[], Ty, str[])` in `ty.aipl`), grammar, formatter and `type_name` read and write
`(A) !prints -> B`. Nothing yet produces or checks an effect: lambdas and named functions used as
values are typed with none, and the pinner's `agree` ignores the list.

### 2. The pinner checks and infers
- [ ] `agree`/unification of two function types compares the lists (today the pinner's patterns
      ignore them), with subsumption below.
- [ ] A lambda checked against a function type may perform that type's effects and no others;
      without an expected type its effects are inferred from its body.
- [ ] A named function used as a value is typed with its signature's effects (the refusal of an
      effectful one is lifted).
- [ ] A call through a local, a field or a method-style call on a function value needs the type's
      effects in the caller.
- [ ] `agree`/coercion accept a function type with a subset of the effects.
- [ ] Error cases in `tests/cases/effects/`: a pure slot given an effectful lambda, an effect not
      declared by the caller of a function value, a subset accepted.

### 3. Monomorphization and closures
- [ ] `apply$n` declares its signature's effects (`closure_conversion` already keys a signature by
      parameters, return and effects).
- [ ] Specialization effects (`specialization_effects`) agree with the types.

### 4. Codegen
- [ ] An indirect call performs its type's effects through the shim slots as any call does; the
      comment on `compile_indirect_call` (no effect check) is updated.

### 5. The todo app and docs
- [ ] The docs site and `write-aipl` skill describe the syntax.
- [ ] `projects/todo` handlers may print or use `gui` in a type that says so.

## Done when

`fn run(f: (i64) !prints -> i64, x: i64) !prints -> i64 { f(x) }` checks, calling `run` from a
function that does not declare `!prints` is an error, a pure lambda is accepted for `f`, and the
whole corpus is green.
