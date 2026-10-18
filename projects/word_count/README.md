# word_count

Counts the lines, words and bytes in files, like `wc`. A word is any run of
non-whitespace characters, so words are separated by whitespace of any kind and
any amount, and leading or trailing whitespace separates nothing.

```sh
$ ./aipl build main.aipl
wrote main
$ ./aipl run -- some-file.txt
2 3 14 some-file.txt
```

The counts agree with `wc`.

## Usage

```
word_count [-lwc] <file>...
```

Prints one row per file: its line, word and byte counts, then its name, separated
by single spaces. `-l`, `-w` and `-c` pick which of the three to show (they
combine, as in `-lw`); the order is always lines, words, bytes. With more than one
file a `total` row follows.

Exits 0 if every file was counted. Exits 1 — after saying why — for an unknown
option, for no file at all, or for a file that could not be read; the other files
are still counted.

Not like `wc`: there is no `-m` (nothing counts characters yet, so it is refused
rather than answered as bytes), no standard input, and the columns are not padded.

## Layout

| file | what it is |
|---|---|
| `main.aipl` | the counting functions, with their tests; and `main`, the command-line shell around them |
| `aipl` | the compiler that builds this project — see [../README.md](../README.md) |

The counting (`word_count`, `line_count`, `counts`) and the formatting (`row`,
`parse_args`) are pure functions on strings and carry the test suite in their
`.test` blocks, which is why the tests need no fixture files: `./aipl check` runs
them from strings. `main` is the thin part — read the files, print the rows, pick
an exit code.
