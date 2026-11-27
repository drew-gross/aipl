# Closures: function values that capture

Goal: a lambda stored in data (`Button("Remove", |model| model.without(item))`) keeps the values it
mentions, so React-style `onClick` handlers close over their props. Captures are copies of the
values at the moment the lambda is made; mutating a captured binding is a later design.

## Context

- A function value is an 8-byte code address (`ConcreteType::Fn`, `is_inline_slot`), called with
  `call_indirect` (`compile_indirect_call`). It owns nothing, so it needs no drop.
- A lambda in value position is lifted with an empty capture list (`typed_infer.rs`, `lower`:
  `lift_lambda(params, body, &[], ..)`), so a capture is an unknown identifier in the lifted
  function (`unknown identifier "$inl0_n"`). A lambda handed to a function that takes a function is
  different: `lifted_argument` computes captures with `typed_lambda_captures` and the callee is
  specialized, so no function value exists at run time.
- `lift_lambda` already takes captures: they become trailing parameters of the lifted function.

## Decisions

- **Defunctionalize in mono; codegen is untouched.** For a function type that has a capturing
  lambda in value position, every value of that type becomes a synthesized variant
  `Closure$<signature>` with one case per lambda or named function of that signature, the case
  carrying the captures. A call through such a value is a call to a synthesized
  `apply$<signature>(closure, args..)` that matches on the case and calls the lifted function with
  the captures appended. Layout, refcounting, drop, copy and pattern code are the variant's, so
  nothing in codegen or the runtimes changes, and captured heap values are released with the closure.
- **Per signature, opt in.** Types of a signature with no capturing lambda stay code addresses, so
  nothing existing changes shape or moves a baseline row.
- **The conversion is a pass after the work-list drains**, because the cases of a signature are
  known only once every lambda has been lifted: it rewrites types and the three expression forms
  (lambda value, function name as value, call through a local or field) in every instance, then adds
  the variants and the `apply` functions.
- **Captures are by value.** A captured `mut` binding is an error naming the binding, until
  mutation has a design. A closure is made at run time as the case holding its captures; a case is
  one per lifted function, so two closures from one lambda share a case.
- Equality on a closure stays unsupported.

## Stages

Stage 1 is built: a capturing lambda stored in a variant, a struct field or a `let` works
(`closure_conversion.rs`, cases `tests/cases/functions/closures.aipl` and `closure_values.aipl`). A
captured `mut` binding is refused in the pinner (`reject_mutable_captures`), which is also where a
lambda's other refusals are made. A function may return a function: the lambda it ends in (or
`return`s) is typed from the declared return type (`typed_returned_lambdas`), and a local or call
result handed where a callee wants a function is wrapped as `|a| g(a)` (`expand_local_functions`).

### 2. The todo app uses it
- [ ] `Element` carries `on_click: (Todo[]) -> Todo[]` generically (`Element<M>`); `view` builds
      handlers that close over the item; the loop finds the clicked key and applies its handler.
- [ ] `GUI_TODO_APP.md` stage 4 updated to say handlers, not `clicked` by key.

### 3. Follow-ups
- [ ] `==` on a type holding a closure, arrays and optionals of functions, and mutating
      captures, each its own decision.

## Done when

`fn make(n: u64) -> Node { Button(|m| { m + n }) }` compiles and runs correctly under `aipl run`,
`--jit` and `aipl build`, the cases above pass with no leak, and `projects/todo` builds its buttons
with `on_click` closures.
