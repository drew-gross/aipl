# A todo app on the `gui` effect

Goal: `projects/todo` becomes a real desktop app — a window with a text field to add items,
a checkbox to tick one, a button to remove one — written in AIPL, on a `gui` effect that grows only
as far as the app needs.

## Context

- The `gui` effect has four operations, all shimmable. `show_view(title, view)` shows a view given in
  its text form in the one window, opening it if need be, and returns. `next_event()` waits for what
  the window has to tell the program, as text: `close_requested` (the close button or Cmd-W),
  `quit_requested` (Cmd-Q) or `closed`. `close_window()` closes it. `show_window(title, text)` shows
  some text and waits until the window is closed, granting every request to close it. The text form is one element a line, children indented two spaces; its grammar and what
  each element becomes are described at the top of `crates/aipl-codegen/src/gui_cocoa.rs`, which
  also reads it (without allocating), makes the one window and the main menu (Quit, Cmd-Q; Close
  Window, Cmd-W) and a delegate class that turns the window's requests into events, and builds AppKit
  views: `NSStackView` for `column` and `row`,
  labels as attributed strings, buttons, checkboxes, text fields and separators. The JIT and the AOT
  runtime both include that file; AppKit is loaded with `dlopen` on the first call and driven through
  `objc_msgSend`, so nothing is linked. `next_event` runs a hand-written `nextEventMatchingMask`
  loop until the window has an event for the program; the runtime closes nothing of its own accord.
- The AIPL side is in `projects/todo`: `elements.aipl` has the `Element` variant and `to_text`, with a
  `gallery` command that draws every element from constants, and `events.aipl` has the `Event`
  variant, its parser and `wait_until_closed`, the loop that grants requests to close and quit.
- Only the window's own requests reach the program. Buttons, checkboxes and fields are drawn and do
  nothing, and a view's `key`s are accepted and unused. A window needs the process's main thread: a
  built binary has it, `aipl check` and `--jit` do not (a real window there is an error), so the app
  is tested with the operations shimmed.
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

- **React is the model to design from.** A view is a function of the model: small AIPL functions
  that take props and return elements, composed the way components are. Every element has a `key`,
  and showing a new view reconciles it with the one on screen by key and kind, touching only what
  differs. Events go up as data naming the element's key, where React would call a handler, and state
  lives in one place, the model, which the inputs display (controlled). Where AIPL cannot follow yet
  — closures cannot cross to the runtime, and there are no hooks to give a component state of its
  own — the model and the loop stand in, and a feature is designed so that the React form can replace
  them when the language allows.
- **Data crosses, closures do not.** The program tells the runtime what the window holds, and the
  runtime tells the program what happened. The program owns its loop: it reads an event, updates its
  model and describes the window again. No AIPL function is ever called from the runtime.
- **A view crosses as text.** The runtimes read a `str` and nothing else, so a view is written in a
  line-based text form by AIPL and read by the runtime, and an event will come back as text that
  AIPL reads. The typed `Element` and `Event` are AIPL's.
- **A program is a loop around `next_event`**, the one call that waits, and a test supplies a script
  of events to it and records the views it was shown, so the whole app runs without a display. This is
  the shape an `await` on a stream of events takes once the language has async: `show_view` and
  `close_window` stay as they are and `next_event` becomes the awaited call, so nothing about the
  operations has to change for it.
- **The runtime grants nothing.** A request to close or to quit is an event, not an action, so a
  program with unsaved changes can ask first; `wait_until_closed` is what a program with nothing to
  ask uses.
- **Real windows only in a built binary**, on the main thread. The project's README says so.
- macOS only, like the rest of the project.

## Open questions

- **A `run_app(model, update, view)` helper.** *Recommended:* yes, written in AIPL in the `gui`
  library over the two operations, so it needs no runtime support. Settled once two apps have
  written the same loop.
- **Text events.** *Recommended:* controlled fields, as in React: a field shows the model's text and
  sends `Changed(key, text)` on every keystroke, which the program writes back into its model, and
  `Submitted(key)` on Enter. Setting a field's text from the model must not fight the user's typing
  (write it only when it differs, keep the cursor), which is what makes this the hard stage-2 item.

## Stages

Order: 0 can go on at any time; 1 first of the rest; 2, 3 and 5 are independent of each other after
it; 4 needs 1–3; 6 needs 1.

### 0. The look of the elements

Goal: the elements `show_view` draws look right, settled by looking at `todo gallery` and changing
what is wrong.

- [ ] Alignment, spacing, field and button widths, font sizes and the window's size and minimum
      size, each looked at in the gallery and fixed where it is off.
- [ ] More elements or options the todo app turns out to need (a heading style, a color for an
      overdue item, a bordered box) added to the text form, the runtime's reader, `elements.aipl`
      and the gallery together.
- [ ] `Element` and `to_text` moved from `projects/todo/elements.aipl` into the `gui` library, so any
      program imports them, once the elements stop changing.

Watch: the text form is written by AIPL and read by Rust, so a change to it is a change to both and to
the grammar at the top of `gui_cocoa.rs`; the parser's tests and `elements.aipl`'s encoder tests each
hold half.

### 1. Clicks

Goal: a button the program has drawn tells it when it is clicked.

- [ ] `Clicked(key)` in the event text form and `Event`; each view made keeps its `key`.
- [ ] `gui_cocoa.rs`: a button's target and action, on the delegate class that already hears the
      window, push the sender's key on the event queue.
- [ ] A counter app in a case, driven by shimmed `show_view` and `next_event`.
- [ ] The counter run by hand in a built binary.

Watch: the queue and the class belong to the main thread, so they need no locks, but a call off it
must fail loudly rather than corrupt AppKit. The run loop must block, not spin. `shims::
every_effect_is_shimmable` and `shim_slots_match_runtime` fail until the slot counts agree.

### 2. Text input and checkboxes

Goal: a text field whose Enter gives an event with its text, and a checkbox whose toggling gives one.

- [ ] Text field and checkbox in the view's text form, and `Changed(key, text)`, `Submitted(key)`
      and `Toggled(key, on)` events.
- [ ] Replacing the window's contents reconciles by key and kind, as React does: an element with a
      key and kind already on screen is updated in place, one with a new key is made, one gone is
      removed; so the field keeps its focus and cursor.
- [ ] A shim-driven case that types, submits and ticks.

Watch: rebuilding every view on each `show_view` loses the first responder and the cursor; the
keyed reuse is the point of this stage, not an optimization. Two elements with one key in one
parent are the program's error and are refused, not guessed at.

### 3. Layout

Goal: contents that fit the window and scroll when there are too many.

- [ ] A scrolling container.
- [ ] The window resizes without clipping, and a field or separator grows with it.

### 4. The todo app

Goal: `projects/todo` is the interactive app.

- [ ] A model, `update` and `view` over `Todo`, `Command` and the file logic already there; the
      view is a field to add, a row per item (checkbox, text, remove button) and a footer with how
      many are left, the row a function of one item and keyed by it, as a React list item is.
- [ ] Every change is saved to `todos.txt`; the file is read when the app starts.
- [ ] `todo` with no command opens the window; the command-line commands stay or go (settle with
      the user once the window covers them).
- [ ] The whole interaction is a test: scripted events in, the views shown and the file written out.

Watch: `read_file_to_string` says nothing about why it failed, so an unreadable file is read as
empty and the next save overwrites it; an error kind would let the app refuse instead.

### 5. Behaving like an app

- [ ] An Edit menu (cut, copy, paste, select all), which text fields need to take them.
- [ ] The Dock's and the system's own requests to quit (logging out) reach the program as
      `quit_requested`, not as the process ending, through the application's delegate.
- [ ] A window the program has asked a question of (unsaved changes) does not close until it says.

### 6. `run_app`

- [ ] The helper in the `gui` library, if the open question is settled yes; the todo app and the
      counter use it.

## Done when

`./main` built from `projects/todo` opens a window where an item is added with a text field and
Enter, ticked with a checkbox and removed with a button; the list is saved after each change and
reloaded at the next start; closing the window or Cmd-Q ends the program with exit 0; and the whole
interaction is covered by `.test` blocks that run with no display.
