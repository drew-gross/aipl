# A todo app on the `gui` effect

Goal: `projects/todo` becomes a real desktop app — a window with a text field to add items,
a checkbox to tick one, a button to remove one — written in AIPL, on a `gui` effect that grows only
as far as the app needs.

## Context

- The `gui` effect has four operations, all shimmable. `show_view(title, view)` shows a view given in
  its text form in the one window, opening it if need be, and returns. `next_event()` waits for what
  the window has to tell the program, as text: `clicked <key>` (a button was clicked),
  `close_requested` (the close button or Cmd-W), `quit_requested` (Cmd-Q) or `closed`. `close_window()` closes it. `show_window(title, text)` shows
  some text and waits until the window is closed, granting every request to close it. The text form is one element a line, children indented two spaces; its grammar and what
  each element becomes are described at the top of `crates/aipl-codegen/src/gui_cocoa.rs`, which
  also reads it (without allocating), makes the one window and the main menu (Quit, Cmd-Q; Close
  Window, Cmd-W) and a delegate class that turns the window's requests into events, and builds AppKit
  views (a window made by `show_view` opens at 480 by 360 points, or as big as its contents need up to
  960 by 720, and cannot be made smaller than 240 by 160; a label gives way to a narrow window and is
  cut short with an ellipsis, so long text cannot hold a window wide; `show_window`'s window is as
  big as its text; the application's icon, in the Dock and the Cmd-Tab switcher, is drawn at run time:
  a white glyph on a rounded tile, the glyph the root's `icon="…"` or else the first letter of the
  program's name, the tile's color taken from the name): `NSStackView` for `column` and `row`,
  labels as attributed strings, buttons, checkboxes, text fields and separators. The JIT and the AOT
  runtime both include that file; AppKit is loaded with `dlopen` on the first call and driven through
  `objc_msgSend`, so nothing is linked. `next_event` runs a hand-written `nextEventMatchingMask`
  loop until the window has an event for the program; the runtime closes nothing of its own accord.
- The AIPL side is in `projects/todo`: `elements.aipl` has the `Element<M>` variant, `to_text` and
  `click`, with a `gallery` command that draws every element from constants, and `events.aipl` has
  the `Event` variant, its parser and `wait_until_closed`, the loop that grants requests to close and
  quit. An `Element<M>` is a view of a model `M`; a `Button` holds what a click does as a function of
  the model, `Button("Add", |todos| { .. })`, the way React's `setState(todos => ..)` is given one,
  and a button is named in events by its `key`, which is where it is in the tree (`b0.2.1`) until one
  is written. `main.aipl` has `todo_app`, a function of the list that answers the window (a
  component `item` per row, a handler per button that closes over what it needs), and `interact`,
  the loop the todo window runs in: an event is looked up in the tree last shown (`click`), which
  answers the new list, which is drawn again; the list lives only as long as the window. Its buttons
  add an item ("Add your first todo" on an empty list, "Add a todo" under the items of any other,
  each with placeholder text) and remove one.
- What reaches the program is the window's own requests and button clicks. A button has to have a
  `key` (up to 64 letters, digits and `- _ . : /`), which names it in its `clicked` event; checkboxes
  and fields are drawn and do nothing. A window needs the process's main thread: a
  built binary has it, `aipl check` and `--jit` do not (a real window there is an error), so the app
  is tested with the operations shimmed.
- A window must be made on the process's main thread. A built binary's `main` is on it. `aipl
  check` and `--jit` run programs on worker threads, so a real `gui` operation cannot run there:
  everything about the app has to be testable with the operations shimmed.
- The runtimes read a `str` a program hands them (`str24`) and no other structured value: an
  array of structs or a variant tree has no reader on the Rust side (the same gap holds back
  `http_get` request headers).
- `projects/todo` is an in-memory list of `Todo`s, `todo_app` the view of it, shown with `show_view`:
  the model, the view and the handlers on its buttons that the interactive app is built on. Nothing is
  saved.

## Decisions

- **React is the model to design from.** A view is a function of the model: small AIPL functions
  that take props and return elements, composed the way components are. Every element has a `key`,
  and showing a new view reconciles it with the one on screen by key and kind, touching only what
  differs. Handlers are closures written on the elements, as in React; what goes up to the program is
  the element's key, which `click` looks up in the tree last shown to find the handler. State lives in
  one place, the model, which the inputs display (controlled). Where AIPL cannot follow yet — there
  are no hooks to give a component state of its own — the model and the loop stand in, and a feature
  is designed so that the React form can replace them when the language allows.
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

Order: 0 can go on at any time; 2, 3 and 5 are independent of each other; 4 needs 2 and 3; 6 can
come once `interact` and `wait_until_closed` are the same loop with different updates.

### 0. The look of the elements

Goal: the elements `show_view` draws look right, settled by looking at `todo gallery` and changing
what is wrong.

- [ ] Alignment, spacing, field and button widths and font sizes, each looked at in the gallery and
      fixed where it is off; a label squeezed by a narrow window runs about 12 points into its
      container's padding, and the window's default and minimum sizes are a first guess.
- [ ] More elements or options the todo app turns out to need (a heading style, a color for an
      overdue item, a bordered box) added to the text form, the runtime's reader, `elements.aipl`
      and the gallery together.
- [ ] `Element` and `to_text` moved from `projects/todo/elements.aipl` into the `gui` library, so any
      program imports them, once the elements stop changing.

Watch: the text form is written by AIPL and read by Rust, so a change to it is a change to both and to
the grammar at the top of `gui_cocoa.rs`; the parser's tests and `elements.aipl`'s encoder tests each
hold half.

Watch: AppKit pins a window's width to what its contents want if any view in them has an opinion about
its own width that is held above the window's priority (500): a separator whose width follows its
container's did, until its horizontal hugging and compression resistance were both set to the lowest
(it opened tall and narrow and could not be widened, only heightened). A new element whose size
follows its container's needs the same. None of this can be tested without a display; look at a window
opened at its default size, widened and narrowed.

### 2. Text input and checkboxes

Goal: a text field whose Enter gives an event with its text, and a checkbox whose toggling gives one.

- [ ] Text field and checkbox in the view's text form carry keys that name them, and give
      `Changed(key, text)`, `Submitted(key)` and `Toggled(key, on)` events, through the delegate the
      buttons already use.
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
- [ ] The window resizes without clipping, and a field or separator grows with it; contents that
      grow after the window is open (a longer list) make it bigger, as far as the opening limit.
- [ ] A view can say how big its window opens and how small it may be made, for a program whose idea
      of a usable size is not the default.

### 4. The todo app

Goal: `projects/todo` is the interactive app.

- [ ] The view is a field to add, a row per item (checkbox, text, remove button) and a footer with
      how many are left, the row a function of one item and keyed by it, as a React list item is.
      Done but for the field and the checkbox's handler, which need stage 2: the rows, their
      handlers and the footer.
- [ ] The whole interaction is a test: scripted events in, the views shown. Done for the buttons
      there are.

### 5. Behaving like an app

- [ ] An Edit menu (cut, copy, paste, select all), which text fields need to take them.
- [ ] A program can give its icon as a picture, not only a glyph: a `str` cannot carry PNG bytes, so
      it is a path to an image file or a text encoding of one.
- [ ] The Dock's and the system's own requests to quit (logging out) reach the program as
      `quit_requested`, not as the process ending, through the application's delegate.
- [ ] A window the program has asked a question of (unsaved changes) does not close until it says.

### 6. `run_app`

- [ ] The helper in the `gui` library, if the open question is settled yes: `wait_until_closed` and
      `interact` are both a loop of `next_event`, an update and a redraw, and become one function of
      a model, an update and a view.

### 7. Persistence

Not part of the work on the window itself; the list can be kept once the interface is settled.

- [ ] The list is read when the app starts and written after each change.
- [ ] An item's text cannot hold a newline if the file is a line per item (`[ ] text`, `[x] text`),
      or the format escapes it: a text with one made the file unreadable.
- [ ] The save does not leave a half-written file if the program dies partway.

Watch: `read_file_to_string` says nothing about why it failed, so an unreadable file read as an empty
list would be overwritten by the next save; an error kind (missing or unreadable) would let the app
refuse instead.

## Done when

`./main` built from `projects/todo` opens a window where an item is added with a text field and
Enter, ticked with a checkbox and removed with a button; closing the window or Cmd-Q ends the
program with exit 0; and the whole interaction is covered by `.test` blocks that run with no display.
