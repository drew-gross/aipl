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
- **A function that performs an effect goes only where the parameter type says so.** `map`,
  `filter` and the other builtins that are fused or specialized take pure functions only; a user
  function's parameter typed `(A) !prints -> B` takes an effectful one. So a function argument is
  held to the effects of the type it is given, exactly as a stored one is, and the effects of a
  specialized copy are those of its bound functions' types. (Fusion and specialization of
  effectful arguments are not worth their cost yet.)
- **Closures keep working as variants:** the signature a closure variant is made for includes the
  effects, so `apply$n` declares the effects of its type and the calls inside it are well-formed.
- **Shims** need nothing new: a shim stands in for an operation at run time wherever it is called.

## Open questions

- Whether a type variable can stand for an effect set (`<E> (A) !E -> B`). *Recommended:* not yet;
  specialization already makes higher-order functions polymorphic over the effects they are handed.

## Stages

Order: 3, then 4.

Stages 1 and 2 are built. Pinner: a lambda stored in a field, constructor or `let` with a function
type may perform that type's effects (`value_arg`); with none it has the effects its body performs
(`inferring`); a call through a local, a field or a method on a function value needs its type's
effects declared (`require_effect`); `agree` accepts fewer effects where more are allowed, and a
named function handed to a callee that is specialized for it still performs its effects in the
caller. After the pinner effects have no run-time meaning: codegen compares function types without
them, closure conversion makes one variant per parameters and return and its `apply` function
performs the union of its cases' effects (`ConcreteFn::effects_by_type`, so a call to it is not
declared again). Stage 1 is built: `Fn` carries `effects: Vec<String>` (sorted, each once, set by the reader in
`ffi_ast.rs`) in `ast::Type`, `ConcreteType`, `Ty`, `ConcreteTy` and `It`, and the AIPL AST
(`Fn(Ty[], Ty, str[])` in `ty.aipl`), grammar, formatter and `type_name` read and write
`(A) !prints -> B`. Nothing yet produces or checks an effect: lambdas and named functions used as
values are typed with none, and the pinner's `agree` ignores the list.

### 3. Hardening
- [ ] A function value typed with effects in a position that is a type variable or an element of a
      generic (`Option<(A) !prints -> B>`-like shapes the checker allows): cases, and the message
      when the effects of two branches differ.
- [ ] The wording of the error for calling a function value (`fn "f" has effect ...`) names it a
      function value, and the one for a named function given to a pure parameter says why
      (`expected (i64) -> _, got (i64) !prints -> i64` does not).

### 4. The todo app and docs
- [ ] The docs site and `write-aipl` skill describe the syntax.
- [ ] `projects/todo` handlers may print or use `gui` in a type that says so.

## Done when

`fn run(f: (i64) !prints -> i64, x: i64) !prints -> i64 { f(x) }` checks, calling `run` from a
function that does not declare `!prints` is an error, a pure lambda is accepted for `f`, and the
whole corpus is green.
