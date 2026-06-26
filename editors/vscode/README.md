# AIPL syntax highlighting for VS Code

Syntax highlighting and basic language configuration (comments, bracket
matching, auto-closing pairs) for the [AIPL](../..) language.

## Layout

```
editors/vscode/
├── package.json              # extension manifest
├── language-configuration.json
├── syntaxes/
│   └── aipl.tmLanguage.json  # TextMate grammar (the single source of truth)
└── README.md
```

The grammar at `syntaxes/aipl.tmLanguage.json` is also exercised by the
crate's `cargo test --test highlighting`, which runs a tiny TextMate
interpreter over every `.aipl` file in `tests/cases/**` and `examples/` and
verifies the scope assigned to each lexed token. Edits to the grammar
should be validated by that test.

## Local install (development)

VS Code loads any extension placed under `~/.vscode/extensions/<name>/`.
For a quick try-out:

```pwsh
# Windows — copy or symlink this directory into VS Code's extensions dir
New-Item -ItemType SymbolicLink `
  -Path "$env:USERPROFILE\.vscode\extensions\aipl-0.1.0" `
  -Target (Resolve-Path .)
```

Reload VS Code (`Developer: Reload Window`), open any `*.aipl` file, and
highlighting should kick in.

## Packaging

To produce a `.vsix` you can install with `code --install-extension`,
install `vsce` (`npm i -g @vscode/vsce`) and run:

```sh
vsce package
```

from this directory.
