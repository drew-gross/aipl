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
import { parse, Json } from json;                          // a builtin *library*: a bare word, where a path is quoted
import { tabulate } from tabulate;
# Doc comment: `#` lines directly above the item. `//` is a plain comment.
pub fn f<T: any>(self: T[], n: i64 = 0, sep: str* = []) !prints -> T? { .. }.test({ assert(..); })
struct P { x: i64, y: i64 = 0 }
variant Shape = Circle(r: i64) | Square(i64) | Empty
```
- Unused imports are errors. `.test`/`.benchmark` blocks follow the body; helper `fn`s inside a block come first.
- Bounds: `any`, `ord`, `variant`. First param named `self` makes `x.f(y)` ≡ `f(x, y)`. Predicates: `fn is_x(self: T) -> bool`.
- Param/let destructuring: `fn f(P { x, y as b })`, `let (a, b) = t;`, `let Circle(r) = s else { return none; };`.
- `fn main() { .. }` void; effects (`!prints`, `!read_files`, ..) must be declared by callers too.

## Types
`i64 u64 u8 .. bool char str`, floats (`import { IEEE754Float64 as Float } from builtins;`, or `as F32`/`as F64` when a file uses both; never mixed with ints), `T[]`, `T?`, `T!E`, `T[,]` grid, `#{T}` set (`#>{T}` `#<{T}` ordered), `#{K: V}` dict,
`(A, B)` tuple, `(A) -> B` function (`(A) !prints -> B` when calling it performs effects, which the caller must declare; a function with fewer effects fits where more are allowed), `Case<V>`. `()` unit.

## Expressions
```
let x = 1;  mut y = 0;  set y = y + 1;  set y++;  set xs.push(x);   // `mut` binding required to set
[1, 2]  [..a, x] (a spread takes an array, or an optional as zero or one elements)  0..n  xs[1..3]  xs[i] (→ T?)  #{1, 2}  #{k: v}  (a, b)  P { x: 1 }  P { ..p, x: 2 }
some(x) none ok(x) err(e)   f()?  (postfix: propagate none/err)   f(sep=", ")   |x| x + 1
`text {expr} \{literal brace}`   """raw multi-line, dedented"""   'c'   "s"
if (c) { a } else { b }      if (let some(v) = o) { .. }
match (x) { some(v) => v, Circle(..) | Square(..) => 0, _ => 1 }      // `Ctor(..)` ignores payload; arms may be `A | B`
for (let x : xs) { .. }   for (let i, x : xs) { .. }   while (c) { .. }   return v;
xs$f()  ≡ xs.map(|e| e.f())      xs$field  ≡ xs.map(|e| e.field)
```
- Blocks: statements end in `;`, optional trailing expression is the value. A construct used as a value
  (`let v = match ..`) may not assign to outer bindings; as a statement (`match ..;`) it may.
- `.join(sep=", ", final_sep=" and ", only_sep=" and ")` writes English lists; `leading_sep=true` / `trailing_sep=true` write `sep` before the first part and after the last (`cells.join(sep="|", leading_sep=true, trailing_sep=true)` is `"|a|b|"`). `T?[]` flattens with
  `.collect_ok()` (`xs$f().collect_ok()`); a function that finds at most one thing returns `T?`, never `T[]`.
- `return` inside a lambda leaves the lambda.
- An array pattern may stand inside a ctor or tuple pattern (`let TupleLit([left, right]) = e else { .. };`, `Pair([a, b]) => ..`), but without a trailing `..`; `return` not allowed inside a match arm expression; a bare `none` can't be a tuple slot.
- Struct patterns rename with `as` (`P { x as px }`, `Pushes { elements, rest as end }`), not `x: px`. A struct pattern names only the fields it reads (`let P { x } = p;` ignores the rest); `field as _` is refused. `struct`/`variant` are never `pub`. No `is_none`: write `!o.is_some()`.
- The case of `is_case`/`as_case`/`collect_arms` is a type argument: `x.is_case::<Circle>()`, `x.as_case::<Circle>()` (`Circle?`), `xs.collect_arms::<Circle>()` (`Circle[]`); the constructor as an argument is refused. A case is a type, so `c: Circle` takes one, and its named payload slots are fields: `c.r`, `let Circle { r } = c;`.
- `fn f<V: variant, A: arm of V>(..)` is generic over one case of `V`; write it as a type argument (`xs.f::<Circle>()`, or `::<Shape, Circle>`) when no argument says which.
- A type argument can refine a generic struct's parameter: `self: Expr<Assign>` holds an `Expr` whose `kind` is known to be an `Assign` (what `body.descendants_of_kind::<Assign>()` returns), and a parameter is destructured in place with the arguments before the fields, `fn f(Expr<Assign> { kind })`; then `let Assign(target, value, _) = kind;` needs no `else`.
- A parameter `f: (A) -> T*` takes a function returning `T[]`, `T?` or one `T` (a lambda or a function's name; the body sees `(A) -> T[]`). Only a parameter's function type may end in `*`; the lint drivers in `lint_hit.aipl` use it, so a lint may return `LintHit?` or `LintHit[]`. A lambda mixing the shapes (`return []` and a single value) is a type error.
- `xs.map(f)` over a named function is flagged: write `xs$f()`. A method call needs the fn's first param named `self`.
- **A test hangs from the function it covers** (`fn f() {..}.test({ .. })`). A test with no function to cover (a language
  feature, not a function) is a top-level `test({ .. })`: rare, reported as `test block N` (its order among the file's), no `#` docs above it.
  Never use a `main` or a dummy function as a place to put tests.
- **A `.test` / `.benchmark` body declares no effect**: it cannot print, read or write files, read stdin or the clock, or run a program unless it wraps that in a `shim` inside the body. Write the stand-in inline, binding every operation of the effect (`read_files`: `read_file_to_string`; `write_files`: `write_string_to_file`; `list_files`: `list_files`; `clock`: `now_nanos` + `monotonic_now`; `prints`: `print` + `put`). A closure records calls with `set log.push(..)` on an outer `mut`, it cannot `return` or use `?`, and the shim is then a statement (`shim .. { .. };`). To exercise the real API, keep a `main` and pin it with `stdout` / `file:` / `expect file:` sections (`tests/cases/files/write_file.aipl`, `tests/cases/builtins/monotonic_now.aipl`).
- Debug with `trace(expr)` (remove before handoff). `assert` works in tests only.

## Operators (import `builtin as op`)
`+ - * ++ --`: `wrapping_add` / `saturating_add`, `wrapping_sub`.., `wrapping_mul`, `wrapping_increment`..;
`/` `saturating_divide`, `%` `saturating_remainder`; `== !=` `equal not_equal`; `< > <= >=` `less_than greater_than
less_than_or_equal greater_than_or_equal`; `&& ||` `logical_and logical_or`; `!` `logical_not`. Strings and arrays are joined with templates, spread literals (`[..a, ..b]`), `push` and `extend`; there is no concat operator.

## Builtins (import by name from `builtins`)
- **text/chars**: print put to_str split words trim pad trim_while starts_with ends_with contains is_digit to_digit is_space is_whitespace is_all_whitespace int_parse int_parse_unsigned inline_str heap_str rope_str
- **arrays**: distinct (first-appearance order; `==`, so not floats) len is_empty is_nonempty first last only push extend reserve reverse sort sort_by drop_first drop_last drop_n drop_last_n remove_index zip zip_with intersperse repeat range tuple_windows cartesian_product
- **query/fold**: map filter filter_map map_join map_reduce reduce left_fold right_fold find_if find_index find_map any all count count_if count_while join collect_ok try_map split_map
- **optional/result**: value_or value_or_err is_some is_some_and is_err_and map_ok map_err
- **dicts**: to_dict get contains_key insert_key remove_key map_to_dict
- **sets** (`import { .. } from sets;`, not `builtins`): to_set to_array union union_all has difference intersection set_map
- **variants**: is_case as_case case_of case_name same_case collect_arms
- **grids**: grid_of grid_from grid_get grid_try_set width height is_in_bounds_of
- **io/terminal**: read_file_to_string write_string_to_file list_files read_line read_byte read_key execute_program clear_terminal move_to
- **`from network`**: `http_get` (`url.http_get(timeout_seconds=30)` → `HttpResponse!Error`, effect `!http_get`; `HttpResponse { status: u16, headers: HttpHeader[], body: str }`, `HttpHeader { name, value }`; import the types from `network` too). Any response is `ok`, a 404 included; `err(message)` is no response (DNS, connect, TLS, timeout, a body that is not text). Redirects are followed, only `http`/`https`. `aipl run` builds a native binary (`--jit` for in-process); shim with `shim http_get { http_get = |url, timeout_seconds| .. }`. Example: `projects/curl`.
- **`from json`**: parse (`Json!str`; `Json` cases Null Bool Number String Array Object(Member[]), `Member { key, value }`; numbers stay text)
- **floats**: render (`x.render(digits=1)`: `to_str` with keyword options; digits after the point, halves to even), to_float (int to float, typed by its position, default F64) float_add float_sub float_mul float_div float_equal float_not_equal float_less_than float_greater_than float_less_than_or_equal float_greater_than_or_equal (import `name as op`; a file has one `==`, so use `equal(a, b)` for non-floats beside `float_equal as ==`; unary `-` needs `float_sub as -`; floats are not `ord`, not set/dict keys, and a struct holding one has no `==`)
- **`from tabulate`**: tabulate, `Column { header, side = Right }` (`rows.tabulate(columns)` → the table as one `str`, a line per row; `side=Left` right-aligns, `Left`/`Right` come from `pad`'s exports)
- **misc**: min max hash now_nanos measure Measurement Span
- **floats**: `x.render(digits=2)` (fixed decimals) or `x.render(significant=2)` (significant digits: 12345 -> 12000, 1.2345 -> 1.2, 0.0012345 -> 0.0012), both half-to-even on the exact value
