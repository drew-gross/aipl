# word_count

Counts the words in a file. A word is any run of non-whitespace characters, so
words are separated by whitespace of any kind and any amount, and leading or
trailing whitespace separates nothing.

```sh
$ ./aipl build word_count.aipl
wrote word_count
$ ./aipl run word_count.aipl main some-file.txt
```

It agrees with `wc -w`.

## Usage

```
word_count <file>
```

Prints the count and exits 0. Prints why and exits 1 if no path was given, or if
the file could not be read.

## Layout

| file | what it is |
|---|---|
| `word_count.aipl` | `word_count`, the counting function, with its tests; and `main`, the command-line shell around it |
| `aipl` | the compiler that builds this project — see [../README.md](../README.md) |

`word_count` is a pure `str -> u64` and carries the whole test suite in its
`.test` block, which is why the tests need no fixture files: `./aipl check` runs
them from strings. `main` is the thin part — read the file, print the number,
pick an exit code.
