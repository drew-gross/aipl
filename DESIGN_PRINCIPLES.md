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
