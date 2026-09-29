# Projects

Real programs written in AIPL. Each subdirectory is a **standalone project**: it
is complete on its own, and the only thing distinguishing it from a project in
its own repository is that it does not have one.

## What a project directory holds

```
word_count/
  aipl              the compiler that builds this project
  aipl.build        which compiler that is (this repository's policy — see below)
  word_count.aipl   the program
  README.md         how to build and run it
```

The compiler is checked in beside the source. That is the design, not a
convenience — see **§4, "A project carries its own compiler"** in
[DESIGN_PRINCIPLES.md](../DESIGN_PRINCIPLES.md). The short version: a project
that names a compiler *version* leaves you to go and make that version be the
one on your `PATH`, which is the problem pyenv, virtualenvs and containers all
exist to solve. A project that carries the compiler has nothing to resolve and
nothing to activate.

So every project is built and run with its own copy, from inside its own
directory:

```sh
cd word_count
./aipl check                     # run the project's tests
./aipl build word_count.aipl     # link a native binary
./aipl run word_count.aipl main <args>
```

Nothing in here refers to the repository around it. A project can be copied out
to its own repository and will keep working, unchanged.

## Two rules that come from living in this repository

A real project upgrades its compiler when it chooses to. The projects here do
not get that latitude, because they are also the language's worked examples:

- **The checked-in compiler is built from this repository's current sources.**
  `projects::checked_in_compilers_are_current` compares the fingerprint recorded
  in `aipl.build` against the compiler's source tree. When a compiler change
  makes it stale, `cargo handoff` re-copies it; by hand it is
  `cargo test --test compiler -- --ignored projects::fill_project_compilers`.
  `aipl.build` is policy machinery, not part of a project — see
  [DESIGN_PRINCIPLES.md](../DESIGN_PRINCIPLES.md) §4.
- **The source is canonically formatted**, like every other checked-in `.aipl`
  (`fmt::all_aipl_files_stay_formatted`).

Each project's own tests run in CI through its own compiler
(`projects::projects_pass_their_own_check`), which is the only thing that
exercises the shipped copy at all.

## Adding a project

Make the directory, write the source, and run the fill helper above to put a
compiler in it. Both tests pick it up automatically — they walk `projects/` — so
there is no list to add it to.
