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
