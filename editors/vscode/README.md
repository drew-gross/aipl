# AIPL for VS Code

Language support for [AIPL](../..): syntax highlighting, go-to-definition,
hover, the document outline, formatting, and diagnostics.

Everything but the highlighting comes from the compiler. The extension starts
`aipl lsp` — the language server built into the compiler itself — and forwards
the editor's questions to it, so what VS Code says about a program is what
`aipl check` says about it. There is no second implementation of name
resolution, formatting or type checking here to drift out of step.

| feature | how |
|---|---|
| syntax highlighting | the TextMate grammar in `syntaxes/`, generated from the lexer's own rules |
| go to definition | the server, via `aipl-index` — crosses files through `import` |
| hover | the declaration's signature and its `# ..` documentation |
| outline / breadcrumbs / symbol search | the file's declarations, with a `variant`'s cases nested under it |
| format document | `aipl fmt`, which is canonical — it decides the layout, not your settings |
| diagnostics | the loader and the type checker, re-run shortly after you stop typing |

Format-on-save is enabled for `.aipl` files by default (the formatter is
canonical, so there is nothing to disagree with); override it in your settings
under `"[aipl]"` if you would rather not.

## Which compiler it runs

A project carries the compiler that builds it (see
[DESIGN_PRINCIPLES.md](../../DESIGN_PRINCIPLES.md) §4), and the editor should
report what *that* compiler reports. So the extension looks, in order:

1. `aipl.serverPath`, if you set it. `${workspaceFolder}` and `${userHome}` are
   expanded — VS Code does not expand them in plain settings itself.
2. `./aipl` in the workspace folder — the compiler the project ships.
3. `./target/debug/aipl`, then `./target/release/aipl` — a checkout of the
   compiler itself, where the compiler it contains is the one you just built.
4. `aipl` on `PATH`, for a file opened outside any project.

A candidate is only used if it **has an `lsp` subcommand**, which the extension
checks by reading its `--help`. That is not a formality: a checkout holds more
than one compiler and they are not the same age — a `target/release` binary
from last month sits beside the `target/debug` one built five minutes ago, and
nothing about either path says which of them can serve. If none can, the error
message lists every path it tried and why each was passed over.

A multi-root workspace gets one server, from the first folder that carries a
usable compiler; set `aipl.serverPath` if that is the wrong one.

Set `aipl.trace.server` to `messages` or `verbose` to watch the traffic in the
**AIPL** output channel.

## Layout

```
editors/vscode/
├── package.json              # extension manifest
├── tsconfig.json
├── language-configuration.json
├── src/
│   └── extension.ts          # starts the server; picks which compiler to run
├── syntaxes/
│   └── aipl.tmLanguage.json  # TextMate grammar (generated — see below)
└── README.md
```

The grammar is **generated** from `crates/aipl-codegen/src/highlight_aipl.aipl`,
which is the lexer's own rule table plus the few things a lexer cannot know.
Edit that file, not this one, and regenerate with
`cargo test --test compiler -- --ignored highlighting::fill_tmlanguage`;
`highlighting::checked_in_tmlanguage_is_current` fails when the two disagree,
and the rest of `tests/suites/highlighting.rs` validates the result against
every token of every case file and example.

The server lives in `crates/aipl-lsp` and is exercised by
`tests/suites/lsp.rs` — including an end-to-end test that spawns `aipl lsp` and
holds a real conversation with it, so the protocol this client speaks is the one
that is tested.

## Building

```sh
npm install        # once
npm run compile    # or `npm run watch` while editing
```

## Local install (development)

VS Code loads any extension placed under `~/.vscode/extensions/<name>/`, so a
symlink is enough:

```sh
# macOS / Linux
ln -s "$PWD" ~/.vscode/extensions/aipl-0.2.0
```

```pwsh
# Windows
New-Item -ItemType SymbolicLink `
  -Path "$env:USERPROFILE\.vscode\extensions\aipl-0.2.0" `
  -Target (Resolve-Path .)
```

Reload VS Code (`Developer: Reload Window`) and open any `*.aipl` file. If the
server does not start, the error message names the command it tried.

## Packaging

```sh
npm i -g @vscode/vsce
vsce package
```

`vsce` runs `npm run compile` first (`vscode:prepublish`) and bundles
`vscode-languageclient` with the extension. The compiler is *not* bundled: the
extension finds one as described above.
