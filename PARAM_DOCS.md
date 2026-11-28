# Per-parameter docs

Goal: a function parameter takes `# ..` lines of its own, above it inside the parentheses, as a
variant case's payload slot already does (`CaseParam::doc`):

```
# The array without the element at `index`.
fn remove_index<T: any>(
    self: T[],
    # Where the element is; at or past the end removes nothing.
    index: u64,
) -> T[] { .. }
```

`aipl docs` and the language server's hover show each parameter's text beside it.

## Context

- A variant case's slot is the precedent: `case_param` in `grammar_aipl.aipl` is
  `Then([Many(Term(DocComment)), OneOf([..])])`, `lower_case_param` reads the doc lines off the
  node's own tokens, `CaseParam.doc` carries them (`ffi_ast.rs` reads it), the walker's
  `case_params` keeps them as hard lines so a documented slot breaks the list to one per line.
- The `param` rule's lowering (`lower_param`, `pattern_param`) reads the node's own tokens by
  position, so a doc line in front of them would shift every index. The docs go on a wrapper rule
  around the existing shapes instead.

## Decisions

- **Syntax:** `# ..` lines above a parameter, inside the parentheses; a blank `#` line is a
  paragraph break, as in every doc block. Nothing else about a parameter changes.
- **`Param.doc: Option<String>`**, the lines joined as `doc_block` joins them.
- **A lambda's parameters take no docs**; only `fn` parameters do.

## Stages

A parameter's docs reach `ast::Param::doc`, `aipl_index::Symbol::slots` (as a case's slots do), `aipl doc`
(`fn.parameter`), the docs site (a list under the function's docs) and hover. `remove_index` is the
one builtin that uses them.

### 1. The builtins
- [ ] The builtins whose parameters are not obvious document them (`join`'s separators, `map_join`,
      `tabulate`'s `Column`), as `remove_index` does.

## Done when

The builtins that need it use per-parameter docs; then delete this file and its `CLAUDE_TODO.txt`
line.
