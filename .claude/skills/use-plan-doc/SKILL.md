---
name: use-plan-doc
description: Run a multi-round piece of work from a plan document in the repo — write the plan (context, committed decisions, numbered stages with checklists), point at it from one line of CLAUDE_TODO.txt, keep it forward-looking as work proceeds, and delete it when the main thrust is done. Use when a task will take several handoffs or sessions, when asked to "make a plan doc", or when resuming work that a plan doc already describes.
---

# Using a plan doc

A plan doc is how a project too big for one task stays on course across rounds and
sessions. It says where the work is going and what is left, and nothing else. Name the
file for the work in capitals at the repo root (`TESTS_AS_FUNCTIONS.md`), the way the
repo's other design documents are.

## Writing the plan

Write it before the first round, and check it with the user when a design decision in it
is theirs to make. It holds:

- **Context.** What the code does today and why it needs to change — enough that someone
  who has not seen the conversation can start. Say what the code does by reading it, not
  from memory.
- **Committed decisions.** The design as decided, with the reason for each where it is not
  obvious. A decision still open goes under an "Open questions" heading, with what would
  settle it.
- **Stages.** Numbered, each a unit that ends at a green `cargo handoff` and stops there.
  A stage has a goal in a sentence, a checklist of todos (`- [ ]`), and what to watch
  while doing it: tests that may change, callers to check, bootstrap hazards. A stage that
  needs substages numbers them (`2.1`, `2.2`). Stages are listed in the order they were
  planned, not a promise of the order they run in: say which depend on which, and let the
  rest be taken as conditions allow.
- **Done when.** The outcome that ends the plan, stated so it can be checked.

Add one line to `CLAUDE_TODO.txt`, under the category the work belongs to, in its terse
style, pointing at the doc and saying what it is for. One line, not one per stage: the
stages live in the doc.

## Working from it

At the start of each round, read the doc, take a stage (or the part of one that can be
finished and verified), and do it. The plan is a guide, not a contract; when what you find
contradicts it, change the plan first and then continue.

At the end of each round, in the same change as the code:

- Tick what is done.
- **Keep the doc forward-looking.** Move, split, reorder, reword and delete items as
  conditions change; leave no trace of where an item used to be. No "moved to stage 3", no
  "was previously", no struck-out items.
- **Keep no log.** The doc does not say what a session did, how many rounds it took, or
  what went wrong on the way; git has that, and so does the round's report to the user.
  A fact that outlived the round because it changes what comes next belongs in the
  context, the decisions or a stage's "watch" list, in the present tense.
- **Prune completed items.** Delete ticked items that no one planning the rest needs. Keep
  only those that explain a decision or are depended on by what is left (a ticked item that
  says what the next stage can now assume is worth keeping). A stage whose items are all
  done and not depended on goes entirely.
- Update the decisions when the design changed, and say why in a clause where a reader
  would otherwise wonder.

Do not run the work from the doc alone: the project's own rules still apply (handoff at the
end of a round, no commits, and so on). The doc says what to do next; it does not replace
those.

## Finishing

When the main thrust of the plan is done — the "done when" holds — delete the doc and delete
its line from `CLAUDE_TODO.txt` in the same change. A finished plan left in the repo is
clutter that reads as work still owed.

Before deleting it, sort what is left over:

- A minor follow-up, or something recommended but not required, becomes **its own line in
  `CLAUDE_TODO.txt`**, written to stand alone (it will be read without the doc), under the
  category it belongs to. One line per item.
- Anything still required is not a leftover: the plan is not done, so keep the doc and the
  stages that hold it.
- Open questions that were never answered and no longer matter are dropped.

Tell the user in the round's report that the plan is complete and was removed, and list the
lines that went into `CLAUDE_TODO.txt`. If the user would rather keep the doc (as a record
they want to read), they will say so; do not keep it on your own judgment.

## Shape of a good plan

- Short enough to read at the start of every round. If a stage's checklist runs past a
  screen, it is two stages.
- Every checklist item is something that can be seen to be done: a function that exists, a
  test that passes, a field that is gone. "Clean up X" is not one.
- Stages that are risky to order wrongly say so (a bootstrap, a format change that needs two
  rounds), with the order that works.
- The numbers are labels, not a schedule. They stay stable while the doc is edited, so a
  reference to "stage 3" in the doc or a message keeps meaning the same stage; renumber only
  when a stage is deleted outright, and never to close a gap if that would leave a message
  pointing at the wrong one.
