# Design principles

The rules that govern the design of AIPL. When making decisions about how to
design APIs, refer to this document.

Each section states the principle, and includes the API decisions that are
downstream of that principle.

---

## 1. The language cannot abort

**No expression in AIPL can terminate the program.** Every operation is total:
for every input it has an answer, and the answer is a value. There is no
trapping, no panicking, no `exit`, no undefined behaviour to be caught later.

This is not "errors are values" — that is a separate and weaker claim, about how
*failure* is represented. This is stronger: the operations that in most languages
are not failures at all, and so have no error channel to use, must still answer.
Integer division is the type case. `a / b` is not a fallible operation the way
opening a file is; it is arithmetic, and arithmetic returns numbers. So it
returns a number for `b == 0` too.

### Downstream decisions

| Operation | Could have | Does |
|---|---|---|
| `a / 0`, `MIN / -1` | trap, or return `i64?` | `saturating_divide`: both answer `MAX` |
| `a % 0` | trap, or return `i64?` | `saturating_remainder`: answers `a` |
| `MIN % -1` | trap | answers `0`, the true remainder |
| `xs[i]` out of range | trap, or clamp | yields `none` — indexing is `T?` |
| `s[i]` | trap | yields `char?`, same rule |
| `s[a..b]` out of range | trap | clamps to the string |
| `int_parse` on garbage | trap, or return a sentinel | yields `i64?` |
| integer overflow | trap | the operator names the wrap: `wrapping_add` vs `saturating_add` |

The overflow row is the one that shows the principle is not merely "pick a
default and move on". Integer overflow has *two* defensible total answers, so rather than
choosing one silently, the user is given the opportunity (and oblication) to choose: `+` has
no meaning until a file imports `wrapping_add as +` or `saturating_add as +`.

### The arithmetic answers are chosen to stay coherent

Being total is necessary, but coherency is also desirable. `/` and `%` for example,  are
connected by an identity:

```
(a / b) * b + (a % b)  ==  a
```

With `a / 0 == MAX`, the only remainder that keeps that true is `a % 0 == a`:
`MAX * 0 + a` is `a`. That is why the pair is `saturating_divide` / `saturating_remainder`
rather than a saturating divide next to some unrelated modulus.

`MIN % -1` is where the identity runs out: it answers `0`, the true remainder,
and the identity does not hold, because the real quotient is not representable at
all. `MIN / -1` saturating to `MAX` had already conceded that. The rule is that
where coherence is available it decides the answer, and where it is not, totality
trumps coherence.

### What it buys, beyond not dying

The payoff most worth knowing is that **totality is what lets the optimizer move
code**. If an expression can abort, its position in the program is observable:
hoisting it into a branch that does not run turns a program that dies into one
that returns, and sinking it out of one does the reverse. So an abortable
expression pins every binding that reaches it, transitively through calls.

### The exceptions, named

- **Out of memory panics the runtime.** `alloc_buffer` and its siblings in
  `str24.rs` do `assert!(!raw.is_null(), "out of memory")`. This is a genuine
  hole: an allocation failure is an input-dependent abort. It is unaddressed
  rather than justified — an OOM-returning-a-value design would have to thread a
  failure through every allocating operation, which is a much larger change than
  making division total. Worth writing down as debt, not as a decision.
- **Unbounded recursion overflows the stack.** No depth limit, no trampolining
  outside the tail-call path. Same status as OOM: a hole, not a decision.

### What it does not mean

It does not mean programs cannot fail. A `main` returning `!Error` prints
`error: <msg>` to stderr and exits 1; a `main` returning `i64` sets the exit
code. Both are ordinary returns. The principle constrains how an *expression* behaves, not
whether a program can report that it did not work.

It also does not mean every operation returns an optional. Reaching for `T?`
everywhere would be unusable. The choice at each site is between a total answer
in the value domain (`a % 0 == a`, clamping a slice) and a total answer in the
type domain (`xs[i]` is `T?`), and it turns on whether the out-of-range case
is something a caller plausibly wants to *branch* on. An index out of bounds
usually is. A zero divisor usually is not — the caller who cares tests `b == 0`
themselves, and the one who does not should not pay an unwrap.

---

## 2. `check` is the whole gate, and it reports everything it can

Two halves, and they are the same idea from opposite ends.

**Bare `aipl check` runs every validation there is.** Not formatting *or* lints
*or* type checking *or* the tests — all of them, every time, from one command.
Flags that narrow the run to a subset are fine and useful; what is not fine is a
validation that only runs if you know to ask for it. A check you have to remember
is a check that will be forgotten, and the tree it was guarding breaks anyway.

**A failing validation must not suppress the others.** If formatting fails, type
checking still runs. If a lint fires, codegen still runs and the tests still run.
The output is everything wrong with the tree, found in one pass — not the first
thing wrong with it.

The reason is the round trip. A gate that stops at the first failure turns one
run into as many runs as you have distinct problems, each paying the full cost of
compiling the world, and each hiding whatever came after it. A developer who
fixes a lint and then discovers a type error, fixes that and then discovers a
failing test, has paid three times for information that was available the first
time. Worse, they have been making decisions with a partial picture: the right
fix for the lint is sometimes different once you know the type error exists.

### Downstream decisions

| Validation | Conforms | Behaviour |
|---|---|---|
| formatting | yes | an unformatted file is named on stderr, its tests still run, and the summary ends `N files need formatting` |
| lints | **no** | a lint hit fails the load, so codegen and the file's tests never run |
| parse errors | **no** | a file that does not parse contributes nothing else |
| type errors | **no** | one bad function stops the whole file, including tests for unrelated ones |

The formatting row is the shape the rest should take: a real failure, reported
precisely, that costs the run its exit code and nothing else.

### The staged version of "as much as it can"

This is a direction, not a plan — nothing here is scheduled, and the rows above
are allowed to keep saying **no** for as long as they do. What the principle
settles is which way each of them should move when someone touches it, so that a
change that makes a failure *more* contagious is recognizable as going the wrong
way even if it is locally convenient.

The three non-conforming rows are not one piece of work, and they get harder in
order:

1. **Lints should be non-fatal to the rest of the run.** A lint is a style
   judgement about code that already parsed and type-checked, so nothing
   downstream of it actually depends on it. This is the cheap one, and it is
   pure sequencing.
2. **A parse error should cost only what it actually broke.** With a recovering
   parser, a file with one bad function still yields the others, and their tests
   still run. This is what the parser library work (`PARSER_LIBRARY.md`) makes
   possible — error recovery is named there as the thing `NestedIn` gives the
   driver a place to hang.
3. **A type error should cost only the function it is in.** If `f` does not type
   check, `g`'s tests should still run. This needs failure to be per-definition
   rather than per-file, which reaches into how the checker reports and how
   codegen decides what to emit.

Each is worth doing on its own; none is a prerequisite for the next.

### What it does not mean

It does not mean a check never fails. `check` exits non-zero when anything is
wrong, and that is the whole point of it — the principle is about *how much it
learned* before it did, not about whether it is allowed to be unhappy.

It does not mean every validation belongs in the language. `cargo handoff` sits
above `check` and does things that are properties of *this repository* rather
than of an AIPL program — regenerating checked-in IR, refilling recorded metrics,
keeping a `#[test]` list in step with a directory. Those stay where they are. The
principle governs what it means to ask a program whether it is well-formed.

---

## 3. `import .. as` is the namespace

**The importer names what it imports.** Every name a file uses from outside is
brought in by an `import` list, and any entry in that list may be renamed on the
way in: `import { Literal as Lit } from "./grammar.aipl";`. That one form does
the whole job that qualified paths, module prefixes and naming conventions do in
other languages, and it does it at the only place where a clash is actually
observable — the file that holds both names.

Two consequences, one for the language and one for the code written in it:

- **The language does not grow a namespace feature.** No qualified paths
  (`doc.print`, `doc::print`), no `import * as doc`, no nested modules, no
  visibility scopes finer than "exported from this file or not". Each of those
  is a second mechanism for a problem `as` already solves, and a second
  mechanism means every reader learns both and every tool handles both.
- **A definition takes the plain name for what it is.** A type is not
  prefixed to say which family it belongs to, and a function is not suffixed
  to say which type it works on. `doc.aipl` exports `print`, not `print_doc`;
  `expr.aipl` exports `Str`, not `EStr`. The file the definition lives in
  is its namespace, and a caller who imports two `Str`s writes
  `import { Str as PatStr }` for the one it has to tell apart — paying the
  prefix only where the ambiguity is real, and choosing a name that says what
  the ambiguity *was*.

The reason to push this hard rather than merely allow it is that the
alternative is paid by everyone. A prefix chosen at the definition is carried by
every importer, including the overwhelming majority that import only one of the
things it disambiguates against. A namespace feature is the same cost moved into
the grammar: it lets the definition stay plain, but only by making every use
site spell the path. `as` puts the cost where the clash is and nowhere else.

### Downstream decisions

| Decision | Conforms | Shape |
|---|---|---|
| operators | yes | no operator is ambient; `import { equal as == }` is the *only* way to get one, and where a semantics must be chosen the alias records it: `wrapping_add as +` |
| `Literal as Lit` in the grammar files | yes | a shortening, not a disambiguation — `as` is also how a file picks the name that reads best in *its* code |
| `doc.aipl`'s `print` | yes | a caller that also uses the builtin `print` imports it as `print_doc`; the definition stays plain |
| the AST case families | yes | `Str` is a primitive in `prim.aipl`, an expression in `expr.aipl` and a pattern in `pat.aipl`; `grammar_aipl.aipl`, which holds all three, imports `Str as StrPrim` and `Str as PatStr` and keeps the expression bare |
| `Ast`, the lowering's seam | yes | its cases wrap the node types by name — `Ty(Ty)`, `Expr(Expr)` — so `ast.aipl` imports the *types* as `TyNode`, `ExprNode`, and the grammar imports the *cases* as `ATy`, `AExpr` |
| `FmtError`/`ParseError`/`LexError` | **no** | each lives in its own file, so `Error` with an `as` at the one site that holds two would do |

The **no** row is inherited, not chosen, and the move is in the same direction
as §2's: an edit that touches it should leave the plain name behind it, not add
another prefix beside it.

### A clash inside one file is resolved by making two files

`as` renames at the file boundary, so it cannot separate two names that are
declared in the *same* file — and a constructor that collides with another
top-level name in its own file is not even an error, it is silently shadowed.
So when two definitions in one file want the same name, the answer is not a
prefix and not a compromise name: **move one of them to a new file**, and let
whichever file needs both rename on import. `ast.aipl` was one file holding
`Ty`, `ExprKind`, `Pat` and `Stmt`, each with a string case, and is now nine;
`Sexp` left `grammar_sexp.aipl` because its `Atom` and the token kind's `Atom`
are the same word at two levels.

This is the right direction on its own. A file that holds one idea is one a
reader can hold whole, and its import list is a complete statement of what the
idea depends on — where a file holding six has imports that serve any of them
and a reader who has to work out which. The prefix was doing that work at every
use site; the file does it once, at the top. So a split that a name clash
forces is a split that was worth making anyway, and the rule is worth applying
even where it feels like a lot of files: it is the *file count* that is the
readable unit, not the line count of any one of them.

The one thing the rule does not reach is a name that must serve as both a type
and a constructor in the same file — `Ast`'s `Ty(Ty)`. There the type is what
gets the alias (`import { Ty as TyNode }`), because the case is the file's own
and the type is a visitor.

### What it does not mean

It does not mean names should be short or generic for their own sake. `print`
is right for `doc.aipl` because printing is the file's one job; a file that
exports twenty things still gives each a name that says what it does. The rule
is about not encoding *which module* into the name — the module already says
that.

It does not mean a rename is free for the reader. A file that imports `Str as
PatStr` has made `PatStr` a name a reader must look up, exactly as they would
have had to look up `EStr`. The difference is that this lookup is in the import
list at the top of the file they are already reading, and only in the files
that needed it.
