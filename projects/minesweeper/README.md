# minesweeper

A command-line Minesweeper on an 8 by 8 board with 10 mines.

```sh
$ ./aipl run
  0 1 2 3 4 5 6 7
0 # # # # # # # #
1 # # # # # # # #
2 # #[#]# # # # #
…
arrows move, r reveals, f flags, q quits
```

The arrow keys move the cursor, drawn as `[#]` around the cell it is on; the keys
act on that cell and need no Enter. `r` reveals it, `f` flags it (or takes the flag
off), `q` quits. Revealing a cell with no mines around it opens the whole empty
region and its border. A cell shows `#` hidden, `F` flagged, `.` revealed with
nothing around it, or a digit counting the mines around it. Exits 0 on a win or a
quit.

The game is drawn on the terminal's second screen (`enter_alternate_screen`), the
way a pager or an editor is, and each move redraws the board in place (`move_to`,
`put`; every row is kept the same width so a redraw leaves nothing of the last
board behind). When play ends the screen is put away (`leave_alternate_screen`), so the shell comes back
as it was, prompt included, and a finished game's board, mines and result are
printed there. Keys are read one at a time with `read_key`, which puts the
terminal into raw mode for the length of each call.

`./aipl run` starts a new game; input can be piped, so a game can be scripted:
`printf 'rq' | ./aipl run`. The output of a scripted run carries the control sequences
the terminal operations write. `tests/suites/projects.rs` plays scripted games and
checks the screen switches and the boards in what they wrote.

## The mines

Ten mines are placed at random, but not until the first reveal, so a game cannot be
lost on its first move. The first `r` puts the mines anywhere but the cell under the
cursor and the cells touching it, which makes the cell empty and opens at least it
and its neighbours (a 3 by 3 area, less at an edge or a corner). A flag put down
before then stays. A first `r` on a flagged cell reveals nothing, so it places
nothing either.

`./aipl run -- SEED` plays the same game again for the same whole number and the
same first cell, which is also how the tests get a fixed board. `main` seeds with
`shim random seeded_with_clock()` or `shim random seeded_with_int(seed)`: `!random`
is an effect with no default generator, so `main` cannot declare it and the program
has to say where its numbers come from. `play`, `start_mining` and `place_mines`
declare it. `new_game(width, height)` makes the mine-free board, `place_mines` mines it
at random (`set game.place_mines(count, away_from)`), and `add_mines` puts mines at
given cells, which is how the tests build the boards they need.

## Layout

| file | what it is |
|---|---|
| `main.aipl` | the rules (neighbours, flood fill, flag, win/lose), the rendering and what each key does, with their tests; and `main`, the loop that reads a key and redraws |
| `aipl` | the compiler that builds this project — see [../README.md](../README.md) |

The rules are pure functions on a `Game` value and carry the tests in their
`.test` blocks, so `./aipl check` needs no input. Only `play` reads keys; its own test plays against stand-ins for the terminal and the keyboard.
