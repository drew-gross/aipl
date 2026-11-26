# A todo app on the `gui` effect

Goal: `projects/todo` becomes a real desktop app — a window with a text field to add items,
a checkbox to tick one, a button to remove one — written in AIPL, on a `gui` effect that grows only
as far as the app needs.

## Context

- The `gui` effect has one operation, `show_window(title: str, text: str)`: it opens a window
  with the text centered in it and blocks until the window is closed. It is shimmable like every
  effect. The window is `crates/aipl-codegen/src/gui_cocoa.rs`, which the JIT and the AOT runtime
  both include: AppKit is loaded with `dlopen` on the first call and driven through
  `objc_msgSend`, so nothing is linked. The run loop is a hand-written
  `nextEventMatchingMask` loop that ends when the window is no longer visible.
- Nothing reaches the program from the window: no events, no input, no custom Objective-C
  classes (a button needs a target class made at run time), no menu (so Cmd-Q and Cmd-W do nothing).
- A window must be made on the process's main thread. A built binary's `main` is on it. `aipl
  check` and `--jit` run programs on worker threads, so a real `gui` operation cannot run there:
  everything about the app has to be testable with the operations shimmed.
- The runtimes read a `str` a program hands them (`str24`) and no other structured value: an
  array of structs or a variant tree has no reader on the Rust side (the same gap holds back
  `http_get` request headers).
- `projects/todo` today keeps the list in `todos.txt`, with the item logic (`Todo`, `apply`,
  `render`, parse and serialize of the file) tested in AIPL, and uses `show_window` to display the
  list. That logic is what the interactive app is built on.

## Decisions

- **Data crosses, closures do not.** The program tells the runtime what the window holds, and the
  runtime tells the program what happened. The program owns its loop: it reads an event, updates its
  model and describes the window again. No AIPL function is ever called from the runtime.
- **Two new operations** in the `gui` effect: one to replace the window's contents, one to wait for
  the next event. Both are shimmable, so a test supplies a script of events and records the contents
  it was shown, and the whole app runs without a display.
- **Real windows only in a built binary**, on the main thread. The project's README says so.
- macOS only, like the rest of the project.

## Open questions

- **How a window's contents and an event cross the boundary.** *Recommended:* a line-based text
  form, written and read by AIPL in a `gui` library (typed `Element` and `Event` values for the
  program; the operations take and give a `str`), because the runtimes can read a `str` and nothing
  else. Settled by whether marshalling a recursive variant is wanted for its own sake elsewhere;
  until then the text form costs one small parser each way.
- **A `run_app(model, update, view)` helper.** *Recommended:* yes, written in AIPL in the `gui`
  library over the two operations, so it needs no runtime support. Settled once two apps have
  written the same loop.
- **Text events.** *Recommended:* Enter submits and carries the field's text; a change per keystroke
  waits until something needs it (validation, filtering as you type).

## Stages

Order: 1 first; 2, 3 and 5 are independent of each other after it; 4 needs 1–3; 6 needs 1.

### 1. Events from a window

Goal: a program describes a window with labels and buttons, waits for events and gets a click or the
window closing.

- [ ] `show_view` and `next_event` listed in `SHIMMABLE_EFFECTS` under `gui`, with `SHIM_SLOT_COUNT`
      and both runtimes' `SHIM_SLOTS` raised to match; `Callee` arms, `builtin_table.aipl` entries and
      `BUILTIN_SIGNATURES` declarations.
- [ ] The view's text form and its AIPL encoder, `Event` and its decoder, for a label, a button
      and a column of them; unit tests in AIPL.
- [ ] `gui_cocoa.rs`: a target class made with `objc_allocateClassPair` and `class_addMethod`
      whose action pushes the button's id on an event queue; `next_event` runs the run loop until the
      queue holds one, and gives `Closed` when the window closes.
- [ ] A counter app in a case, driven by shimmed `show_view` and `next_event`.
- [ ] The counter run by hand in a built binary.

Watch: the queue and the class belong to the main thread, so they need no locks, but a call off it
must fail loudly rather than corrupt AppKit. The run loop must block, not spin. `shims::
every_effect_is_shimmable` and `shim_slots_match_runtime` fail until the slot counts agree.

### 2. Text input and checkboxes

Goal: a text field whose Enter gives an event with its text, and a checkbox whose toggling gives one.

- [ ] Text field and checkbox in the view's text form, and `Submitted(id, text)` / `Toggled(id, on)`
      events.
- [ ] Replacing the window's contents keeps the view of an element that has the same kind and id,
      updating it in place, so the field keeps its focus and what was typed.
- [ ] A shim-driven case that types, submits and ticks.

Watch: rebuilding every view on each `show_view` loses the first responder and the cursor; the
keyed reuse is the point of this stage, not an optimization.

### 3. Layout

Goal: contents that fit the window and scroll when there are too many.

- [ ] Rows and columns with spacing and padding (`NSStackView`), and a scrolling container.
- [ ] The window opens sized to its contents and resizes without clipping.

### 4. The todo app

Goal: `projects/todo` is the interactive app.

- [ ] A model, `update` and `view` over `Todo`, `Command` and the file logic already there; the
      view is a field to add, a row per item (checkbox, text, remove button) and a footer with how
      many are left.
- [ ] Every change is saved to `todos.txt`; the file is read when the app starts.
- [ ] `todo` with no command opens the window; the command-line commands stay or go (settle with
      the user once the window covers them).
- [ ] The whole interaction is a test: scripted events in, the views shown and the file written out.

Watch: `read_file_to_string` says nothing about why it failed, so an unreadable file is read as
empty and the next save overwrites it; an error kind would let the app refuse instead.

### 5. Behaving like an app

- [ ] A main menu with Quit (Cmd-Q) and Close (Cmd-W); closing the window ends `next_event` with
      `Closed`.
- [ ] The window comes to the front when launched from a terminal and has the app's name in the
      menu bar.

### 6. `run_app`

- [ ] The helper in the `gui` library, if the open question is settled yes; the todo app and the
      counter use it.

## Done when

`./main` built from `projects/todo` opens a window where an item is added with a text field and
Enter, ticked with a checkbox and removed with a button; the list is saved after each change and
reloaded at the next start; closing the window or Cmd-Q ends the program with exit 0; and the whole
interaction is covered by `.test` blocks that run with no display.
