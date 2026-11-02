---
name: write-aipl
description: Terse AIPL syntax and builtin cheat sheet. Use before writing or editing any .aipl code (compiler sources, lints, tests/cases, examples).
---

# Writing AIPL

Verify with `aipl check <file>` (runs `.test` blocks; `--fix` formats). Examples: `tests/cases/**`.
Full docs: `docs/index.html`; builtin sources: `crates/aipl-mono/src/builtin_*.aipl`; native
signatures: `BUILTIN_SIGNATURES` in `crates/aipl-syntax/src/lib.rs`.

## Files
```
# Module docs: detached `#` block at the very top (blank line under it).
import { map, wrapping_add as +, equal as == } from builtins;   // operators: always `name as op`
import { helper, Struct } from "./other.aipl";
# Doc comment: `#` lines directly above the item. `//` is a plain comment.
pub fn f<T: any>(self: T[], n: i64 = 0, sep: str* = []) !prints -> T? { .. }.test({ assert(..); })
struct P { x: i64, y: i64 = 0 }
variant Shape = Circle(r: i64) | Square(i64) | Empty
```
- Unused imports are errors. `.test`/`.performance` blocks follow the body; helper `fn`s inside a block come first.
- Bounds: `any`, `ord`, `variant`. First param named `self` makes `x.f(y)` ≡ `f(x, y)`. Predicates: `fn is_x(self: T) -> bool`.
- Param/let destructuring: `fn f(P { x, y as b })`, `let (a, b) = t;`, `let Circle(r) = s else { return none; };`.
- `fn main() { .. }` void; effects (`!prints`, `!read_files`, ..) must be declared by callers too.

## Types
`i64 u64 u8 .. bool char str`, `T[]`, `T?`, `T!E`, `T[,]` grid, `#{T}` set (`#>{T}` `#<{T}` ordered), `#{K: V}` dict,
`(A, B)` tuple, `(A) -> B` function, `Case<V>`. `()` unit.

## Expressions
```
let x = 1;  mut y = 0;  set y = y + 1;  set y++;  set xs.push(x);   // `mut` binding required to set
[1, 2]  [..a, x]  0..n  xs[1..3]  xs[i] (→ T?)  #{1, 2}  #{k: v}  (a, b)  P { x: 1 }  P { ..p, x: 2 }
some(x) none ok(x) err(e)   f()?  (postfix: propagate none/err)   f(sep=", ")   |x| x + 1
`text {expr} \{literal brace}`   """raw multi-line, dedented"""   'c'   "s"
if (c) { a } else { b }      if (let some(v) = o) { .. }
match (x) { some(v) => v, Circle(..) | Square(..) => 0, _ => 1 }      // `Ctor(..)` ignores payload; arms may be `A | B`
for (let x : xs) { .. }   for (let i, x : xs) { .. }   while (c) { .. }   return v;
xs$f()  ≡ xs.map(|e| e.f())      xs$field  ≡ xs.map(|e| e.field)
```
- Blocks: statements end in `;`, optional trailing expression is the value. A construct used as a value
  (`let v = match ..`) may not assign to outer bindings; as a statement (`match ..;`) it may.
- `.join(sep=", ", final_sep=" and ", only_sep=" and ")` writes English lists. `T?[]` flattens with
  `.collect_ok()` (`xs$f().collect_ok()`); a function that finds at most one thing returns `T?`, never `T[]`.
- `return` inside a lambda leaves the lambda.
- No list patterns inside ctor patterns; `return` not allowed inside a match arm expression; a bare `none` can't be a tuple slot.
- Struct patterns rename with `as` (`P { x as px }`, `Pushes { elements, rest as end }`), not `x: px`. `struct`/`variant` are never `pub`. No `is_none`: write `!o.is_some()`.
- A case with keyword params (`For`, `Ctor`) can't go through `as_case(Case)` as a function value: destructure with `let Case(..) = x else { return none; };`.
- `xs.map(f)` over a named function is flagged: write `xs$f()`. A method call needs the fn's first param named `self`.
- **A `.test` / `.performance` body declares no effect**: it cannot print, read or write files, read stdin or the clock, or run a program unless it wraps that in a `shim` inside the body. Write the stand-in inline, binding every operation of the effect (`read_files`: `read_file_to_string`; `write_files`: `write_string_to_file`; `list_files`: `list_files`; `clock`: `now_nanos` + `monotonic_now`; `prints`: `print` + `put`). A closure records calls with `set log.push(..)` on an outer `mut` (declare it `str[]` and push; `set x = param` from a closure inside nested shims failed with "undeclared variable"), it cannot `return` or use `?`, and the shim is then a statement (`shim .. { .. };`). To exercise the real API, keep a `main` and pin it with `stdout` / `file:` / `expect file:` sections (`tests/cases/files/write_file.aipl`, `tests/cases/builtins/monotonic_now.aipl`).
- Debug with `trace(expr)` (remove before handoff). `assert` works in tests only.

## Operators (import `builtin as op`)
`+ - * ++ --`: `wrapping_add` / `saturating_add`, `wrapping_sub`.., `wrapping_mul`, `wrapping_increment`..;
`/` `saturating_divide`, `%` `saturating_remainder`; `== !=` `equal not_equal`; `< > <= >=` `less_than greater_than
less_than_or_equal greater_than_or_equal`; `&& ||` `logical_and logical_or`; `!` `logical_not`. Strings and arrays are joined with templates, spread literals (`[..a, ..b]`), `push` and `extend`; there is no concat operator.

## Builtins (import by name from `builtins`)
- **text/chars**: print put to_str split words trim trim_while starts_with ends_with contains is_digit to_digit is_space is_whitespace is_all_whitespace int_parse int_parse_unsigned inline_str heap_str rope_str
- **arrays**: len is_empty is_nonempty first last push extend reserve reverse sort sort_by drop_first drop_last drop_n drop_last_n zip_with intersperse repeat range tuple_windows cartesian_product to_array
- **query/fold**: map filter filter_map map_join map_reduce reduce left_fold right_fold find_if find_index find_map any all count count_if count_while join union_all collect_ok try_map split_map
- **optional/result**: value_or value_or_err is_some is_some_and is_err_and map_ok map_err
- **sets/dicts**: to_set to_dict union has get contains_key insert_key remove_key set_map map_to_dict
- **variants**: is_case as_case case_of case_name same_case collect_arms
- **grids**: grid_of grid_from grid_get grid_try_set width height is_in_bounds_of
- **io/terminal**: read_file_to_string write_string_to_file list_files read_line read_byte read_key execute_program clear_terminal move_to
- **misc**: min max hash now_nanos measure Measurement Span
