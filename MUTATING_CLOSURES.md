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

Order: 4, then 5.

Stages 1, 2 and 3 are built. A mutating closure compiles as a lifted function
`lam(mut self: Env, args)` where `Env` is a synthesized struct holding everything its body mentions
from outside; the lifted body unpacks `self` into locals named as the source names them, runs the
body, and packs them back (`lift_mutating_lambda`). `[mut n = 0]` is `mut n = 0;` around a
`[mut n]` closure, made by the pinner, so mono sees one kind of capture. The closure is a variant
case holding `Env`; a group of closures of one type has `apply$n` (plain call) and, when any of them
mutates, `apply$n$mut` (`mut` closure, answering it as the call left it); `set f(x);` is
`f = apply$n$mut(f, x)`. A call of a mutating closure is marked by the closure's type as its one type
argument. State is only observable through effects until stage 4. A mutating closure cannot be handed
to a function that takes a function (the call is declined with a message saying so), and cannot
`return` early (the write-back would be skipped). Stages 1 and 2: Pinner: `[mut x]` takes a `mut` variable of the enclosing function (not a
loop's from outside the loop) or declares owned state `[mut n = 0]`; the body must `set` each, may
`set` no other `mut` variable, and returns nothing; the closure's type is `() mut ..`; a plain function
fits where `mut` is allowed, never the reverse (`agree`); `set f(x);` lowers to `set f = f(x)`, a call
of a `mut` closure answering the closure, as a mutating method answers its receiver, and `f(x);` alone
is refused as a lost mutation. A variable taken by `[mut x]` is *moved*: the enclosing code may not use
it afterwards (`Binding::moved`). That is flow-insensitive: a move in one branch of an `if` or `match`
is a move for its sibling branches and for what follows. A closure called through a struct field
is refused, and a mutating function is `mut self` as every mutating function is (`fn drain(mut
self: () mut -> ())`, called `set f.drain();`). Valid programs stop in monomorphization with "checked
but not compiled yet" until stage 3. Stage 1 is built: The function type carries `mutating: bool` after its effects in `ast::Type`,
`ConcreteType`, `Ty`, `ConcreteTy` and `It` (and the AIPL `Ty`), parsed as `(A) mut !prints -> B`;
`ExprKind::Lambda` carries `Vec<Capture>` (`name`, `init: Option<Expr>`, `span`), parsed from
`[mut a, mut n = 0]` in front of a lambda and laid out by the formatter. Nothing reads either yet:
the pinner refuses a lambda with a capture list (`reject_captures_for_now`) and types every function
as non-mutating. Two things the syntax needed that were not there: `()` is now the unit type, so a
function type can return nothing, and a closure with no parameters (`|| { .. }`) can be written in
value position.

### 4. Lending
- [ ] A closure over an enclosing variable that is only passed down writes the variable back
      after its last use. A mutating closure is not handed to a higher-order function that is
      specialized for it (`map`, `each`): such a call is refused, as an effectful one is.

### 5. The optimizer
- [ ] `sink`, deferral and fusion read a `set f(x);` as a write to `f` and `[mut x]` as a move;
      cases in `tests/cases/optimizations/` for a call that must not be sunk or dropped.

## Done when

`fn counter() -> () mut !prints -> () { mut x = 0; [mut x] || { set x++; print(`{x}`) } }` checks,
`mut f = counter(); set f(); set f();` prints 1 and 2, a second `counter()` starts again at 1,
using `x` in `counter` after the closure is made is an error, and the whole corpus is green. (Met by
`tests/cases/lambdas/mutating_closures.aipl`; what remains is the lending in stage 4 and the
optimizer in stage 5, after which the plan is deleted.)
