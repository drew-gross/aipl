# todo

A todo list, shown in a window. macOS only. It is a vehicle for building up AIPL's GUI, so what it
does is whatever the window can do so far; [GUI_TODO_APP.md](../../GUI_TODO_APP.md) plans the rest.

```sh
$ ./aipl build main.aipl
wrote main
$ ./main            # opens the list in a window
$ ./main gallery    # opens a window with one of every element, from constants
```

## Usage

```
todo [show | gallery]
```

With no command (or `show`) a window opens with an empty list and an "Add your first todo" button;
once there are items it has an "Add a todo" button under them. Each adds an item with placeholder
text (`My first todo`, `Todo 2`, …), until a window can be typed into. Its checkboxes and fields are
drawn and do nothing yet, and nothing is saved: the list lasts as long as the window. `gallery`
opens a window of every element instead.

Cmd-W and the close button close the window, and Cmd-Q quits, ending the program with exit 0. The
window's icon, in the Dock and when you Cmd-Tab, is a white check mark on a colored tile
(`gallery`'s is a grid).

Exits 1, after saying why, for a command it does not know.

`elements.aipl` is the `Element` type and the text form the window is given them in, and
`events.aipl` is what the window tells the program, with the loop that grants requests to close.
`./aipl check` runs the project's tests; `./aipl build main.aipl` writes the binary. Build it before
showing the window: `./aipl run` and `--jit` run the program off the main thread, which a window
needs.
