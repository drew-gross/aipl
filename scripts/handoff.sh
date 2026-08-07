#!/usr/bin/env bash
#
# scripts/handoff.sh — one-command pre-handoff gate for this repo.
#
# Runs the finish-a-task sequence in dependency order, paying for the *expensive*
# regeneration steps (perf/section refill, dogfood-IR regen — each a full-corpus
# run) only when a test run proves they're needed, and stopping with a pointed
# message on any failure a refill can't fix. One command instead of hand-driving
# six, so it's the whole sequence in a fraction of the tokens.
#
# Order & why:
#   1. Format first — `cargo fmt` (Rust) + the `format_corpus` helper (every
#      checked-in `.aipl`). Formatting is cheap and *span-shifting*, so doing it
#      up front means a formatting-only fix never costs a second full test run.
#   2. Discovery run. Three outcomes:
#        - green                       -> done.
#        - only fillable staleness      -> remediate (steps 3-5), then re-confirm.
#          (a section mismatch, a         Refreshing a section is always
#           drifted per-case #[test]      recoverable — `git reset --hard HEAD` —
#           list, or IR staleness)        so even behavioral sections (stdout /
#                                          exit code / errors / check) are refilled
#                                          and the git diff is the review surface,
#                                          flagged at the end.
#        - a failure a refill can't fix -> STOP (a compile/link error, a crash, a
#                                          failed in-language `.test`, or any
#                                          non-cases test). No section to record
#                                          from, so a refill would just burn a
#                                          full corpus run without fixing it.
#   3. `fill_case_tests` regenerates the checked-in per-case `#[test]` list when a
#      case file was added or removed. First, so a new case can reach step 4.
#   4. `fill_expected`, scoped with `AIPL_CASE` to each mismatched case, refreshes
#      just those cases' sections from actual output (not the whole corpus).
#   5. Staged dogfood-IR regen: fill -> validate -> corpus run against the staged
#      artifact -> auto-promote when that run is green.
#   6. Final run confirms green against the live (promoted) artifacts.
#
# Test runner: `cargo nextest`, which runs each test in its own process (so one
# crashing case can't take the binary down with it) and reports a stable
# `FAIL [..] (n/m) <binary> <test>` line per failure — what the greps below parse.
# Two nextest differences this script has to account for:
#   - It *defaults to fail-fast*, the opposite of `cargo test`, so every
#     whole-suite run below passes `--no-fail-fast` explicitly.
#   - It does not run doctests at all. There are real ones here, so step 1b runs
#     `cargo test --doc` separately rather than let that coverage vanish.
# `--color never` keeps the captured output greppable.
#
# Exit status: 0 = handoff green; 1 = stopped (message says at which step and why).

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
# The dogfood-IR corpus run spawns the compiler as a subprocess whose CWD isn't
# the repo root, so this path must be absolute.
# Two artifacts are staged and promoted together: the parser-hook engine and the
# formatter engine (see FMT_SOURCES in aipl-codegen). Either one pending means an
# interrupted workflow.
STAGED="$REPO/crates/aipl-codegen/src/dogfood.clif.staged"
STAGED_FMT="$REPO/crates/aipl-codegen/src/fmt.clif.staged"
STEP_OUT="$(mktemp -t handoff-step.XXXXXX)"
STEP_OUT_SAVED="$STEP_OUT"
# Per-step wall-clock, so a slow handoff can be attributed to a step rather than
# guessed at. Written as `seconds<TAB>label` and reported at exit.
TIMINGS="$(mktemp -t handoff-times.XXXXXX)"
SCRIPT_START=$SECONDS
trap 'rm -f "$STEP_OUT" "$TIMINGS"' EXIT

bold=$'\033[1m'; red=$'\033[31m'; green=$'\033[32m'; dim=$'\033[2m'; off=$'\033[0m'

# The whole suite. `--no-fail-fast` because nextest cancels on the first failure
# by default and this script needs to see *all* remediable staleness in one pass
# (see the discovery step).
NEXTEST=(cargo nextest run --color never --no-fail-fast)

# One `#[ignore]`d author helper, by exact name. nextest selects ignored tests
# with `--run-ignored only` rather than libtest's `-- --ignored <name>`.
helper() { cargo nextest run --color never --run-ignored only -E "test(=$1)"; }

# Test names from nextest's per-failure summary lines:
#     FAIL [   0.007s] (1/1) aipl::dogfood_ir validate_staged_ir
# The status column also carries abnormal exits (SIGSEGV/ABORT/TIMEOUT), which
# `unfillable` below keys on separately. Deduped because nextest prints each
# failure twice — once inline as it happens, once in the closing summary.
failed_tests() { awk '/^ *(FAIL|SIGSEGV|ABORT|TIMEOUT|LEAK-FAIL) \[/ {print $NF}' "$1" | sort -u; }

banner() { printf '\n%s==> %s%s\n' "$bold" "$*" "$off" >&2; }

# Run a labelled step, capturing combined output to $STEP_OUT. Caller inspects $?,
# so the command's status is passed through untouched.
run_step() {
    banner "$1"
    local label="$1"; shift
    local start=$SECONDS
    "$@" >"$STEP_OUT" 2>&1
    local rc=$?
    local secs=$((SECONDS - start))
    printf '%s    (%ss)%s\n' "$dim" "$secs" "$off" >&2
    printf '%s\t%s\n' "$secs" "$label" >>"$TIMINGS"
    return $rc
}

# Where the wall-clock went, slowest first. Printed on success *and* failure —
# a handoff that stops late has still spent the time, and that's worth seeing.
timing_report() {
    [ -s "$TIMINGS" ] || return 0
    printf '\n%stime: %ss total%s\n' "$dim" "$((SECONDS - SCRIPT_START))" "$off" >&2
    sort -rn "$TIMINGS" | awk -F'\t' -v d="$dim" -v o="$off" \
        '$1 > 0 { printf "%s  %5ds  %s%s\n", d, $1, $2, o }' >&2
}

# Preserve the current step's output past the next step / the EXIT trap, so a
# failure message can point the reader at the full log.
save_out() { STEP_OUT_SAVED="$(mktemp -t handoff-fail.XXXXXX)"; cp "$STEP_OUT" "$STEP_OUT_SAVED"; }

# Abort with a step name and (optionally) the salient excerpt.
fail() {
    printf '\n%sHANDOFF FAILED at: %s%s\n' "$red$bold" "$1" "$off" >&2
    if [ -n "${2:-}" ]; then printf '%s\n' "$2" >&2; fi
    printf '%s(full output: %s)%s\n' "$dim" "$STEP_OUT_SAVED" "$off" >&2
    timing_report
    exit 1
}

# A leftover staged artifact means a previous IR workflow was interrupted; a plain
# plain suite run fails on `no_staged_ir_pending` until it's resolved. Don't guess.
if [ -e "$STAGED" ] || [ -e "$STAGED_FMT" ]; then
    STEP_OUT_SAVED="$STAGED"
    fail "startup" "A staged IR artifact already exists:
    $STAGED / $STAGED_FMT
Resolve the interrupted workflow first — promote it
    cargo nextest run --run-ignored only -E 'test(=promote_staged_ir)'
or discard it
    rm -f '$STAGED' '$STAGED_FMT'"
fi

# --- 1. Format (cheap, up front, span-shifting) --------------------------------

run_step "cargo fmt (Rust)" cargo fmt
[ $? -eq 0 ] || { save_out; fail "cargo fmt" "$(cat "$STEP_OUT")"; }

# Compile everything (incl. every test binary) so a compile error surfaces here
# cleanly rather than buried in a test run's output.
run_step "nextest --no-run (compile check)" cargo nextest run --no-run --color never
[ $? -eq 0 ] || { save_out; fail "compile" "$(grep -nE '^error' "$STEP_OUT" | head -30)"; }

# nextest does not run doctests. Run them here so switching runners didn't
# silently drop them; they're a couple of seconds.
run_step "cargo test --doc (nextest skips doctests)" cargo test --doc
[ $? -eq 0 ] || { save_out; fail "doctests" "$(grep -nE '^(error|test .* FAILED)' "$STEP_OUT" | head -20)"; }

# `format_corpus` rewrites any mis-formatted `.aipl` in place, then fails on
# purpose to show its summary — so its non-zero exit is expected; a genuine
# formatter error prints "format failed:".
run_step "aipl fmt (format_corpus)" helper format_corpus
if grep -q 'format failed:' "$STEP_OUT"; then
    save_out; fail "aipl fmt" "$(grep -n 'format failed:' "$STEP_OUT")"
fi
reformatted="$(grep -oE 'reformatted [0-9]+ file' "$STEP_OUT" | grep -oE '[0-9]+' | head -1)"
if [ -n "${reformatted:-}" ] && [ "$reformatted" -gt 0 ]; then
    printf '%s    (reformatted %s .aipl file(s))%s\n' "$dim" "$reformatted" "$off" >&2
fi

# --- 2. Discovery test ---------------------------------------------------------

# `--no-fail-fast` (baked into $NEXTEST) so the discovery run executes *every*
# test and reports all remediable staleness in one pass. Stopping at the first
# failure would be actively wrong here: a change that makes both the per-case
# `--- performance ---` sections and `dogfood.clif` stale would cancel inside
# `cases`, `need_ir` would stay 0, the staged-IR regen (step 5) would be skipped,
# and the final run would then fail on `checked_in_ir_is_current` with the IR
# never regenerated. Running everything surfaces the section mismatches *and* the
# IR gate together.
run_step "nextest (discovery)" "${NEXTEST[@]}"
if [ $? -eq 0 ]; then
    printf '\n%sHANDOFF OK%s (green with no regeneration needed)\n' "$green$bold" "$off" >&2
    timing_report
    exit 0
fi
save_out  # keep the discovery output for any failure message below

# Failures a refill can't fix (there's no section to record actual output into):
# a case that won't build/link/spawn, a crash, or a failed in-language `.test`.
# Stop rather than burn a full-corpus refill that won't help.
unfillable='(load|compile|emit|link|spawn|instrumented compile) failed:'
unfillable+='|\(in-language tests\) failed:|Abort trap|SIGSEGV|SIGABRT'
# nextest reports an abnormal exit in its status column rather than as a signal
# name in the output, so key on those too.
unfillable+='|^ *(SIGSEGV|ABORT|TIMEOUT) \['
if grep -qE "$unfillable" "$STEP_OUT"; then
    fail "nextest (failure a refill can't fix)" "$(grep -nE "$unfillable" "$STEP_OUT" | head -20)"
fi
# Any FAILED test that isn't a per-case test (each case is its own `#[test]`,
# named `<prefix>_<display path>` for the three case roots — their fillable
# mismatches are handled below), the case-list gate, or a known IR-staleness gate
# is a real test failure. The unfillable grep above has already stopped on a case
# that genuinely broke, so what reaches here is a stale section.
bad="$(failed_tests "$STEP_OUT" \
       | grep -vE '^((cases|examples|crates)_.*|every_case_has_a_test|checked_in_ir_is_current|no_staged_ir_pending)$' \
       || true)"
if [ -n "$bad" ]; then
    fail "nextest (failing test)" "$bad"
fi

# What (fillable) staleness did we see? A backtick-section mismatch (any of
# stdout / exit code / stderr / errors / check / performance / monomorphizations)
# or an error-fixture mismatch is refilled; so is a *missing* required section,
# which is what a brand-new case reports — `fill_expected` creates one that isn't
# there, so a new case finishes in this same pass. The two IR gates trigger an IR
# regen; a drifted per-case `#[test]` list is regenerated.
need_fill=0; need_ir=0; need_case_tests=0
grep -qE '`[a-z ]+` mismatch|error mismatch|missing required `--- [a-z ]+ ---` section' \
    "$STEP_OUT" && need_fill=1
failed_tests "$STEP_OUT" | grep -qE '^(checked_in_ir_is_current|no_staged_ir_pending)$' && need_ir=1
failed_tests "$STEP_OUT" | grep -qx 'every_case_has_a_test' && need_case_tests=1
if [ $need_fill -eq 0 ] && [ $need_ir -eq 0 ] && [ $need_case_tests -eq 0 ]; then
    fail "nextest (unrecognized failure)" "$(tail -40 "$STEP_OUT")"
fi

# Distinct sections that changed (for the review note), and whether any are
# behavioral (worth a closer look at the git diff than a metrics-only refill).
changed_sections="$(grep -oE '`[a-z ]+` mismatch' "$STEP_OUT" | tr -d '`' \
                    | sed 's/ mismatch$//' | sort -u | paste -sd, - | sed 's/,/, /g')"
behavioral_changed=0
grep -qE '`(stdout|stderr|exit code|errors|check)` mismatch|error mismatch' "$STEP_OUT" \
    && behavioral_changed=1

# The exact cases that need refilling. Each mismatch prints `[<display-path>]:
# `<section>` mismatch` (or `error mismatch`), and that bracketed display path is
# precisely what `AIPL_CASE` filters on — so we can refill just the failing cases
# instead of paying for a whole-corpus fill. Dedup: one case may report several
# mismatched sections, and one scoped fill refreshes all of that case's sections.
# (Built with a read loop, not `mapfile`, to stay compatible with bash 3.2.)
fail_cases=()
while IFS= read -r c; do
    [ -n "$c" ] && fail_cases+=("$c")
done < <(grep -oE '\[[^]]+\]: ((`[a-z ]+`|error) mismatch|missing required)' "$STEP_OUT" \
             | sed -E 's/^\[([^]]+)\].*/\1/' | sort -u)

# Cases added since the checked-in `#[test]` list was generated. These are the
# reason a brand-new case used to cost a second handoff: with no `#[test]` yet,
# the discovery run never *executed* them, so their unrecorded sections went
# unseen and `need_fill` stayed 0 — the final run (after step 3 declares them)
# was the first thing to notice. `every_case_has_a_test` already names them, so
# fold them into the fill set and finish them in this pass. Only the
# "with no `#[test]`" half: the other half lists *deleted* cases, which have no
# file left to fill.
while IFS= read -r c; do
    if [ -n "$c" ]; then
        fail_cases+=("$c")
        need_fill=1
    fi
done < <(awk '
    /case\(s\) with no `#\[test\]`:/  { grab = 1; next }
    /declared `#\[test\]`\(s\) with no case file:/ { grab = 0 }
    /^[[:space:]]*$/                  { grab = 0 }
    grab                              { print }
' "$STEP_OUT" | sed -E 's/^[[:space:]]*([^ ]+) \([A-Za-z0-9_]+\)$/\1/' | sort -u)

# --- 3. Regenerate the per-case `#[test]` list ---------------------------------

# Before the refills: a case added without its `#[test]` never ran in discovery,
# so regenerating first gives the fill step (and the final run) a chance to see
# it. A brand-new case whose sections are also unrecorded will still fail the
# final run — that's one more handoff, not a wrong answer.
if [ $need_case_tests -eq 1 ]; then
    run_step "fill_case_tests (per-case #[test] list)" helper fill_case_tests
    grep -qE 'wrote [0-9]+ `#\[test\]`' "$STEP_OUT" \
        || { save_out; fail "fill_case_tests" "$(tail -40 "$STEP_OUT")"; }
fi

# --- 4. Refill changed sections ------------------------------------------------

if [ $need_fill -eq 1 ]; then
    # A scoped `AIPL_CASE` fill runs (and refreshes) only the matched case, so we
    # refill exactly the cases that mismatched instead of the whole corpus. If we
    # somehow couldn't pin down any case path (unexpected message format), fall
    # back to a whole-corpus fill so a mismatch is never left unrefreshed.
    if [ "${#fail_cases[@]}" -eq 0 ]; then
        run_step "fill_expected (section refill — full corpus, no case path found)" \
            helper fill_expected
        grep -q 'refresh complete' "$STEP_OUT" \
            || { save_out; fail "fill_expected" "$(tail -40 "$STEP_OUT")"; }
    else
        # A scoped (`AIPL_CASE`) run diverges with the "filter active" panic, not
        # the whole-corpus "section refresh complete" message — but the refill
        # still happens during the run. Success is that scoped summary line
        # reporting the case was seen (`matched > 0`, else the harness asserts
        # before printing) with `0 failed` (no unfillable failure slipped in).
        for case in "${fail_cases[@]}"; do
            run_step "fill_expected (section refill — $case)" \
                env AIPL_CASE="$case" cargo nextest run --color never \
                    --run-ignored only -E 'test(=fill_expected)'
            grep -qE '\[filter "[^"]*"\]: [0-9]+ passed, 0 failed,' "$STEP_OUT" \
                || { save_out; fail "fill_expected ($case)" "$(tail -40 "$STEP_OUT")"; }
        done
    fi
fi

# --- 5. Regenerate + validate + promote dogfood IR -----------------------------

if [ $need_ir -eq 1 ]; then
    run_step "fill_staged_ir" helper fill_staged_ir
    grep -qE 'wrote .*\.staged' "$STEP_OUT" || { save_out; fail "fill_staged_ir" "$(tail -40 "$STEP_OUT")"; }

    run_step "validate_staged_ir (entry-level pre-check)" helper validate_staged_ir
    [ $? -eq 0 ] || { save_out; fail "validate_staged_ir" "$(tail -40 "$STEP_OUT")"; }

    run_step "staged-IR corpus run (AIPL_DOGFOOD_IR + AIPL_FMT_IR)" \
        env AIPL_DOGFOOD_IR="$STAGED" AIPL_FMT_IR="$STAGED_FMT" "${NEXTEST[@]}"
    if [ $? -ne 0 ]; then
        save_out
        fail "staged-IR corpus run (candidate IR is wrong — diff .staged vs live)" \
            "$(grep -nE 'mismatch|FAILED|Abort' "$STEP_OUT" | head -20)"
    fi

    run_step "promote_staged_ir" helper promote_staged_ir
    grep -q 'promoted' "$STEP_OUT" || { save_out; fail "promote_staged_ir" "$(tail -40 "$STEP_OUT")"; }
fi

# --- 6. Final confirmation against the live artifacts --------------------------

run_step "nextest (final)" "${NEXTEST[@]}"
if [ $? -ne 0 ]; then
    save_out
    fail "nextest (final — regeneration didn't settle)" \
        "$(grep -nE 'mismatch|FAILED|Abort' "$STEP_OUT" | head -20)"
fi

# --- Report --------------------------------------------------------------------

printf '\n%sHANDOFF OK%s\n' "$green$bold" "$off" >&2
if [ $need_fill -eq 1 ]; then
    if [ -n "$changed_sections" ]; then
        printf '  refilled sections: %s\n' "$changed_sections" >&2
    else
        # Only *missing* sections (a brand-new case) — nothing mismatched.
        printf '  filled in missing section(s) for a new case\n' >&2
    fi
fi
[ $need_ir -eq 1 ] && printf '  regenerated + promoted dogfood IR\n' >&2
[ $need_case_tests -eq 1 ] && printf '  regenerated the per-case #[test] list\n' >&2
if [ $behavioral_changed -eq 1 ]; then
    printf '%s  ! behavioral output changed — review the git diff before committing%s\n' \
        "$bold" "$off" >&2
    printf '%s    (git reset --hard HEAD undoes the refill if it recorded a regression)%s\n' \
        "$dim" "$off" >&2
fi
timing_report
exit 0
