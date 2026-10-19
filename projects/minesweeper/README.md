# minesweeper

A command-line Minesweeper. **Scaffolding only**: `main` prints a notice and
exits. The game needs input, randomness and terminal control that AIPL does not
have yet; this file lists them so they can be added one at a time.

```sh
$ ./aipl run
minesweeper: not playable yet, see README.md
```

## Layout

| file | what it is |
|---|---|
| `main.aipl` | the program |
| `aipl` | the compiler that builds this project — see [../README.md](../README.md) |

## Planned design

Keep the rules (board, mine placement, neighbour counts, reveal/flood-fill,
flag, win/lose) as pure functions on values, tested from `.test` blocks like
`word_count`; `main` is the thin shell that reads a command, applies it and
redraws.

## Functions we need

Existing builtins cover strings, arrays, `print` and files. Missing, by effect
or capability:

### Must have

| need | why |
|---|---|
| read a line from standard input (a `!reads_stdin`-style effect), returning `str?` at end of input | the whole input loop; with it a line-based game (`r 3 4`, `f 3 4`) is playable |
| random numbers (a `!random` effect, or a seedable generator value): an integer in `0..n` | place mines. A seedable generator keeps the game testable, with the seed taken from `args` |
| parse a `str` to an integer, with an error on bad input (`builtin_int_parse` exists; check it covers `u64` and whitespace-trimmed input) | read coordinates |
| exit code from `main` after a win or loss | already supported (`-> i64`) |

### Pure helpers (can be written in AIPL today)

| need | why |
|---|---|
| 2-D grid indexing: `index(row, col, width)` and neighbour enumeration with edge clipping | cell lookup, adjacent-mine counts |
| Fisher–Yates shuffle or "pick k distinct indices" given a generator | mine placement that never double-places |
| `split` on whitespace (check what exists) | parse a command line into words |
| string padding / `repeat` for drawing columns | aligned board output |

### Nice to have (real-time terminal play)

| need | why |
|---|---|
| read one key without waiting for Enter (raw mode) | `hjkl`/arrow cursor play instead of typed commands |
| ANSI output: clear screen, move cursor, colour (plain `print` of escape codes works once `"\x1b"` escapes exist; check) | redraw in place, classic number colours |
| terminal size | fit the board to the window |
| a clock (`!clock` exists) for the elapsed-time counter | the usual timer display |
