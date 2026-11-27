# Closures that mutate what they capture

Goal: a closure may `set` variables it captured, the way a `mut self` method sets its receiver, with
the mutation written where it is made (a capture list), where it is typed (`mut` in the function
type) and where it is called (`set f(x);`), so a reader knows what changes and the optimizer knows
which calls it may not elide, sink or defer.

## Context

- A closure is a variant case holding its captures (`closure_conversion.rs`); a capture is a copy
  taken when the lambda is made, and a lambda that reads a `mut` binding is refused
  (`reject_mutable_captures` in `pin.rs`).
- A function type carries the effects a call performs: `(A) !prints -> B`; a function with fewer
  effects fits where more are allowed; calling a value needs the effects declared by the caller.
- A method with a `mut self` receiver mutates it: written `fn f(mut self: T)`, called as a
  statement (`set xs.push(x);`), cannot return a value; at the ABI level it returns the updated
  receiver and the call site stores it back. An expression may not also mutate.
- A `shim` closure may `set` outer `mut` variables: each lives in a cell for the shim's extent,
  the closure becomes a function reading and writing the cells, the shim's body may not use a
  variable the closure sets, and the cells are written back when the shim ends
  (`shim_closures.rs`).
- Last-use analysis exists on the typed tree (`move_last_use_typed.rs`): it decides when a value is
  moved rather than copied.
- Values are refcounted and copy-on-write, so nothing aliases.

## Decisions

- **Capture list, then the lambda:** `[mut total] |x| { set total += x; }`. Naming a variable
  in `[mut ..]` makes the closure mutate it, and the body still has to `set` it; setting a captured
  variable that is not listed is an error, and so is listing one the body never sets. A capture
  not listed is a copy, as now.
- **Owned state is declared in the list:** `[mut n = 0] || { set n++; }`. The state starts as the
  initializer and lives with the closure; copying the closure copies it (COW).
- **`[mut x]` on an existing variable lends it to the closure.** The enclosing code cannot read
  or set `x` from the closure's creation until the closure's last use (including every copy of
  it). If the closure was only passed down, `x` has the updated value after that; if it was
  returned or stored, `x` is dead to the enclosing code for good and the closure owns the state.
  Nothing aliases. A function that makes a counter, `mut x = 0; [mut x] || { set x++; .. }`,
  returns a closure that owns `x`.
- **A mutating closure returns nothing**, as a mutating method does: its body's value is `()`.
- **Type:** `(A) mut -> ()`, with the effects after it: `() mut !prints -> ()`. A function with
  no `mut` fits where one with it is expected, never the reverse (the same subset rule as
  effects).
- **Calling is a statement that sets the closure:** `set f(x);` on a `mut` binding or `mut`
  parameter (`fn drain(mut f: (A) mut -> ())`), like a `mut self` method. A call through a
  non-`mut` binding of a `mut` type is an error.
- **The optimizer** treats `set f(x);` as a write to `f` and the creation of `[mut x]` as a move of
  `x`: nothing is sunk or deferred across them that reads or writes the variable.


## Stages

Order: 2, 3 in turn; 4 after 3; 5 any time after 2.

Stage 1 is built. The function type carries `mutating: bool` after its effects in `ast::Type`,
`ConcreteType`, `Ty`, `ConcreteTy` and `It` (and the AIPL `Ty`), parsed as `(A) mut !prints -> B`;
`ExprKind::Lambda` carries `Vec<Capture>` (`name`, `init: Option<Expr>`, `span`), parsed from
`[mut a, mut n = 0]` in front of a lambda and laid out by the formatter. Nothing reads either yet:
the pinner refuses a lambda with a capture list (`reject_captures_for_now`) and types every function
as non-mutating. Two things the syntax needed that were not there: `()` is now the unit type, so a
function type can return nothing, and a closure with no parameters (`|| { .. }`) can be written in
value position.

### 2. The pinner
- [ ] The refusal of a capture list is replaced by the rules below. A listed variable must be a `mut` binding (or an initialized state); the body may `set` it
      and may not return a value; unlisted sets and unused listings are errors.
- [ ] A mutating lambda has a `mut` type; subsumption allows a plain function where `mut` is
      allowed; calling needs `set f(x);` on a `mut` binding.
- [ ] The lending rule: the enclosing code may not use `x` from the creation of `[mut x]` until
      the closure's last use; a closure that is returned, stored or captured by another has no
      last use inside the function, so `x` is gone.
- [ ] Error cases in `tests/cases/lambdas/`.

### 3. Closures that own their state
- [ ] The captured variables of a mutating closure are one synthesized struct; the lifted function
      takes it as `mut self` and the `apply` function takes the closure as `mut`, matches on the
      case and stores the updated environment back into it.
- [ ] A call `set f(x);` stores the updated closure back into `f`.
- [ ] Cases: a counter factory returned and called twice, two copies of a closure counting
      separately, state of a heap value (an array pushed to), leak gate clean.

### 4. Lending
- [ ] A closure over an enclosing variable that is only passed down writes the variable back
      after its last use. A mutating closure is not handed to a higher-order function that is
      specialized for it (`map`, `each`): such a call is refused, as an effectful one is.

### 5. The optimizer
- [ ] `sink`, deferral and fusion read a `set f(x);` as a write to `f` and `[mut x]` as a move;
      cases in `tests/cases/optimizations/` for a call that must not be sunk or dropped.

## Done when

`fn counter() -> () mut !prints -> () { mut x = 0; [mut x] || { set x++; print(`{x}`) } }` checks,
`let mut f = counter(); set f(); set f();` prints 1 and 2, a second `counter()` starts again at 1,
using `x` in `counter` after the closure is made is an error, and the whole corpus is green.
