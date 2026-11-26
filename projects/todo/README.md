# todo

A todo list kept in a file, and shown in a window. macOS only.

```sh
$ ./aipl build main.aipl
wrote main
$ ./main add buy milk
1. [ ] buy milk

1 left
$ ./main done 1
1. [x] buy milk

0 left
$ ./main            # opens a window listing the items
```

The window only displays the list for now. [GUI_TODO_APP.md](../../GUI_TODO_APP.md) plans the app
it is meant to become, with items added, ticked and removed in the window itself.

## Usage

```
todo [show | add <text> | done <n> | undo <n> | remove <n> | clear-done]
```

With no command (or `show`) the list opens in a window, and the program ends when the window is
closed. The others change the list, save it and print it. An item is numbered as the listing
numbers it, from 1.

The list is the file `todos.txt` in the directory the program runs in: a line `[ ] text` for each
item still to do and `[x] text` for each done. A file that cannot be read is an empty list; one
that has any other kind of line is refused, and nothing is saved.

Exits 0, or 1 — after saying why — for a command that makes no sense, an item that does not exist
or a file that is not a list.

`./aipl check` runs the project's tests; `./aipl build main.aipl` writes the binary. Build it
before showing the window: `./aipl run` and `--jit` run the program off the main thread, which a
window needs.
