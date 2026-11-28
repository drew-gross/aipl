# Shared variables: `[mut x]` captures a variable, not a copy of it

Goal: a closure with `[mut x]` in its capture list and the code that made it see one variable. A
handler stored in a window's tree, `[mut todos] || { set todos = todos.removing(index) }`, changes
the `todos` the loop that draws the window reads, as a React `setTodos` does. The explicitness stays
where it is: the capture list names every variable the closure may change, and the closure's type
says `mut`.

## Context

- A `mut` local that any closure's capture list in the function names is a cell (`Binding::cell` in
  `pin.rs`, decided by name before the body is typed): its declaration makes one
  (`let x$cell = cell_new(v)`), a read is `cell_get(x$cell)`, and a `set` rooted at it reads the
  cell into a local of its name, assigns, and writes it back. A closure that names it captures the
  handle like any local, so closure conversion and lifting treat a `mut` closure as any other.
- The cell calls are one-element array blocks in the code generator (`CellNew`, `CellGet`,
  `CellSet`); nothing in source can write them.
- Calls through function values carry the function's type as their one type argument, which the
  sink and single-use inlining read to leave a call with effects, or one that mutates, where it is.

## Decisions

- **A variable that any capture list names is a cell**: a heap block holding the variable's value,
  made where the variable is declared and shared by the code around it and by every closure that
  captures it. Reading it copies the value out (copy on write as ever); writing replaces the value.
  Copying a closure copies its handle on the cell, so the copy shares the state.
- **Only a `[mut ..]` capture is shared.** A closure that reads a `mut` variable without naming it
  in the capture list is refused, as before.
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

Stage 4 is what is left: cells exist in the code generator, a `[mut x]` local is one, and the optimizer leaves the cell calls and calls through `mut` function types where they are.

### 4. The todo app
- [ ] `Element` takes handlers of type `() mut -> ()` and is no longer generic over the model;
      `interact` owns `mut todos`, a component is given its callbacks as props, and a click is
      looked up in the tree and called; `GUI_TODO_APP.md` and the README say so.

## Done when

`mut n = 0; let f = [mut n] || { set n++; }; f(); f(); n` is 2; a closure copied from another
changes the same variable; `projects/todo` removes an item with
`Button("Remove", [mut todos] || { set todos = todos.removing(index) })` written where `todos` is;
and the whole corpus is green with no leak.
