# minesweeper

A command-line Minesweeper on an 8 by 8 board with 10 mines.

```sh
$ ./aipl run
  0 1 2 3 4 5 6 7
0 # # # # # # # #
…
r ROW COLUMN to reveal, f ROW COLUMN to flag, q to quit
r 0 0
```

Commands are typed a line at a time: `r ROW COLUMN` reveals a cell, `f ROW COLUMN`
flags it (or takes the flag off), `q` quits. Revealing a cell with no mines around
it opens the whole empty region and its border. A cell shows `#` hidden, `F`
flagged, `.` revealed with nothing around it, or a digit counting the mines
around it. Exits 0 on a win or a quit, 1 on hitting a mine.

`./aipl run` starts a new game; input can be piped, so a game can be scripted: `printf 'r 0 0\nq\n' | ./aipl run`. Each redraw clears the screen first, so a scripted run's output carries the clear sequence (`ESC [ 2 J ESC [ H`) before every board.

## The mines

Ten mines are placed at random each game. `./aipl run -- SEED` plays the same
game again for the same whole number, which is also how the tests get a fixed
board. `main` seeds with `shim random seeded_with_clock()` or
`shim random seeded_with_int(seed)`: `!random` is an effect with no default
generator, so `main` cannot declare it and the program has to say where its
numbers come from. `place_mines` declares `!random`; everything else takes the
mines as an argument (`new_game(width, height, mines)`).

## Layout

| file | what it is |
|---|---|
| `main.aipl` | the rules (neighbours, flood fill, flag, win/lose), the rendering and the command parser, with their tests; and `main`, the loop that reads a command and redraws |
| `aipl` | the compiler that builds this project — see [../README.md](../README.md) |

The rules are pure functions on a `Game` value and carry the tests in their
`.test` blocks, so `./aipl check` needs no input. Only `play` reads standard input.

## Still missing

| need | why |
|---|---|
| ownership that reaches `with` through a struct field or a function parameter | `with` writes in place when it is handed the only reference (a fresh array, or a `mut` local at its last use), but the board lives in a `Game` field read through borrowed parameters, so each move copies the 64 marks |
| a cursor driven by `read_key` | the `read_key` builtin returns one arrow key (or other key) without waiting for Enter; `play` still reads typed commands |
| ANSI output: move cursor, colour | redraw in place, classic number colours (the `clear_screen` builtin clears) |
| terminal size | fit the board to the window |
| a clock (`!clock` exists) | the usual elapsed-time counter |
