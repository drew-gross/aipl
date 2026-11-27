# Shared variables: `[mut x]` captures a variable, not a copy of it

Goal: a closure with `[mut x]` in its capture list and the code that made it see one variable. A
handler stored in a window's tree, `[mut todos] || { set todos = todos.removing(index) }`, changes
the `todos` the loop that draws the window reads, as a React `setTodos` does. The explicitness stays
where it is: the capture list names every variable the closure may change, and the closure's type
says `mut`.

## Context

- A closure that mutates is built (`MUTATING_CLOSURES` work, deleted) as owning what it takes:
  `[mut x]` moves `x` into the closure, which holds it in a synthesized struct (`lift_mutating_lambda`),
  and a call is `f = apply$n$mut(f, args)`. The enclosing code may not use `x` afterwards
  (`Binding::moved`), unless the closure is only ever called as `set f(..)`, when it is inlined and
  `x` is lent (`inline_closure`, `lend`).
- Values do not alias: a struct, array or closure is copied on write, so a variable has one owner.
  Nothing here is a reference today, except what the runtime does for a `shim` closure, which keeps
  its captured variables in numbered global slots for the shim's extent (`ShimCellGet`,
  `ShimCellSet`).
- Calls through function values carry the function's type as their one type argument, which the
  sink and single-use inlining read to leave a call with effects, or one that mutates, where it is.

## Decisions

- **A variable that any capture list names is a cell**: a heap block holding the variable's value,
  made where the variable is declared and shared by the code around it and by every closure that
  captures it. Reading it copies the value out (copy on write as ever); writing replaces the value.
  Copying a closure copies its handle on the cell, so the copy shares the state.
- **Only a `[mut ..]` capture is shared.** A variable a closure reads without naming it in the
  capture list is copied when the closure is made, as before.
- **A `[mut x]` names a local, not a parameter**, for now: a parameter holds a copy of what it was
  passed and is no place for state others can see.
- **The enclosing code uses the variable freely**, before and after the closure is made: the
  rules that it is moved or lent go. A read sees what the last write made, whoever made it.
- **State a closure starts with, `[mut n = 0]`, is a cell too**, made when the closure is.
- **A closure that mutates is an ordinary closure at run time.** What it holds does not change when
  it is called, only the cells it holds do; so the environment struct that was packed and unpacked,
  `apply$n$mut` and `lift_mutating_lambda` are replaced by the capturing-lambda machinery the plain
  closures use, the cells being captured values.
- **The type `() mut -> ()` says the call may change variables the caller can see**, and what
  reorders or drops calls treats such a call, and every read and write of a cell, as having an
  effect.
- **Cells are an internal representation**, in the typed tree as three calls: make one, read it,
  write it. They are not a type the language has.

## Settled

- **How a component reaches a variable:** it is handed the callback as a prop, made where the
  variable is (`item(todo, index, remove)`), as in React. No by-reference parameter.
- **Calling one:** a plain call, as a statement (a `mut` closure returns nothing); the `mut` in its
  type and in the capture list carry the explicitness.

## Open questions

- **Inlining a closure that is only ever called** is dropped (it needed the variable lent); it can
  come back as an optimization that leaves the variable in a plain slot when no closure that can
  outlive the call captures it.

## Stages

Order: 1, 2, 3, 4.

### 1. Cells in the code generator
- [x] `cell_new`, `cell_get`, `cell_set` as `Callee` arms, internal builtins and `builtin_table.aipl`
      entries, with codegen (a one-element array block; JIT and AOT share the arms). Nothing in
      source reaches them yet, so stage 2's cases are their first test.
- [ ] The sink and single-use inlining treat them as having effects: this waits for stage 3, since
      nothing produces them before stage 2.

### 2. Cell variables in the pinner
- [ ] A `mut` variable named by a capture list, or declared by one, is a cell: its declaration makes
      one, its reads read it and its writes (`set x = v`, `set x.field = v`, a mutating method
      through `set`, `set x++`) write it.
- [ ] `moved`, `lent` and the inlining are removed with their errors; what a capture list may not
      name stays (a binding that is not `mut`, a name that is not there, a name twice).
- [ ] The cell is the closure's captured value: mono lifts a closure that mutates as it lifts one
      that does not, and closure conversion has no `apply$n$mut` or environment struct.
- [ ] Cases: the counter factory, copies sharing state, an accumulator read after its calls, a
      closure called after the variable is read in between, two closures over one variable, a heap
      value in the cell; leak gate clean.

### 3. The optimizer
- [ ] A call through a `mut` function type is never moved, dropped or merged, and neither is a
      cell read or write (cases in `tests/cases/optimizations/`).

### 4. The todo app
- [ ] `Element` takes handlers of type `() mut -> ()` and is no longer generic over the model;
      `interact` owns `mut todos`, a component is given its callbacks as props, and a click is
      looked up in the tree and called; `GUI_TODO_APP.md` and the README say so.

## Done when

`mut n = 0; let f = [mut n] || { set n++; }; f(); f(); n` is 2; a closure copied from another
changes the same variable; `projects/todo` removes an item with
`Button("Remove", [mut todos] || { set todos = todos.removing(index) })` written where `todos` is;
and the whole corpus is green with no leak.
