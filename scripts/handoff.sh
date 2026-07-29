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
#      up front means a formatting-only fix never costs a second full `cargo test`.
#   2. Discovery `cargo test`. Three outcomes:
#        - green                       -> done.
#        - only fillable staleness      -> remediate (steps 3-4), then re-confirm.
#          (any section mismatch or       Refreshing a section is always
#           IR staleness)                  recoverable — `git reset --hard HEAD` —
#                                          so even behavioral sections (stdout /
#                                          exit code / errors / check) are refilled
#                                          and the git diff is the review surface,
#                                          flagged at the end.
#        - a failure a refill can't fix -> STOP (a compile/link error, a crash, a
#                                          failed in-language `.test`, or any
#                                          non-cases test). No section to record
#                                          from, so a refill would just burn a
#                                          full corpus run without fixing it.
#   3. `fill_expected` refreshes every changed section from actual output.
#   4. Staged dogfood-IR regen: fill -> validate -> corpus run against the staged
#      artifact -> auto-promote when that run is green.
#   5. Final `cargo test` confirms green against the live (promoted) artifacts.
#
# Exit status: 0 = handoff green; 1 = stopped (message says at which step and why).

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
# The dogfood-IR corpus run spawns the compiler as a subprocess whose CWD isn't
# the repo root, so this path must be absolute.
STAGED="$REPO/crates/aipl-codegen/src/dogfood.clif.staged"
STEP_OUT="$(mktemp -t handoff-step.XXXXXX)"
STEP_OUT_SAVED="$STEP_OUT"
trap 'rm -f "$STEP_OUT"' EXIT

bold=$'\033[1m'; red=$'\033[31m'; green=$'\033[32m'; dim=$'\033[2m'; off=$'\033[0m'

banner() { printf '\n%s==> %s%s\n' "$bold" "$*" "$off" >&2; }

# Run a labelled step, capturing combined output to $STEP_OUT. Caller inspects $?.
run_step() { banner "$1"; shift; "$@" >"$STEP_OUT" 2>&1; }

# Preserve the current step's output past the next step / the EXIT trap, so a
# failure message can point the reader at the full log.
save_out() { STEP_OUT_SAVED="$(mktemp -t handoff-fail.XXXXXX)"; cp "$STEP_OUT" "$STEP_OUT_SAVED"; }

# Abort with a step name and (optionally) the salient excerpt.
fail() {
    printf '\n%sHANDOFF FAILED at: %s%s\n' "$red$bold" "$1" "$off" >&2
    if [ -n "${2:-}" ]; then printf '%s\n' "$2" >&2; fi
    printf '%s(full output: %s)%s\n' "$dim" "$STEP_OUT_SAVED" "$off" >&2
    exit 1
}

# A leftover staged artifact means a previous IR workflow was interrupted; a plain
# `cargo test` fails on `no_staged_ir_pending` until it's resolved. Don't guess.
if [ -e "$STAGED" ]; then
    STEP_OUT_SAVED="$STAGED"
    fail "startup" "A staged dogfood-IR artifact already exists:
    $STAGED
Resolve the interrupted workflow first — promote it
    cargo test --test dogfood_ir -- --ignored promote_staged_ir
or discard it
    rm '$STAGED'"
fi

# --- 1. Format (cheap, up front, span-shifting) --------------------------------

run_step "cargo fmt (Rust)" cargo fmt
[ $? -eq 0 ] || { save_out; fail "cargo fmt" "$(cat "$STEP_OUT")"; }

# Compile everything (incl. every test binary) so a compile error surfaces here
# cleanly rather than buried in a test run's output.
run_step "cargo test --no-run (compile check)" cargo test --no-run
[ $? -eq 0 ] || { save_out; fail "compile" "$(grep -nE '^error' "$STEP_OUT" | head -30)"; }

# `format_corpus` rewrites any mis-formatted `.aipl` in place, then fails on
# purpose to show its summary — so its non-zero exit is expected; a genuine
# formatter error prints "format failed:".
run_step "aipl fmt (format_corpus)" cargo test --test fmt -- --ignored format_corpus
if grep -q 'format failed:' "$STEP_OUT"; then
    save_out; fail "aipl fmt" "$(grep -n 'format failed:' "$STEP_OUT")"
fi
reformatted="$(grep -oE 'reformatted [0-9]+ file' "$STEP_OUT" | grep -oE '[0-9]+' | head -1)"
if [ -n "${reformatted:-}" ] && [ "$reformatted" -gt 0 ]; then
    printf '%s    (reformatted %s .aipl file(s))%s\n' "$dim" "$reformatted" "$off" >&2
fi

# --- 2. Discovery test ---------------------------------------------------------

run_step "cargo test (discovery)" cargo test
if [ $? -eq 0 ]; then
    printf '\n%sHANDOFF OK%s (green with no regeneration needed)\n' "$green$bold" "$off" >&2
    exit 0
fi
save_out  # keep the discovery output for any failure message below

# Failures a refill can't fix (there's no section to record actual output into):
# a case that won't build/link/spawn, a crash, or a failed in-language `.test`.
# Stop rather than burn a full-corpus refill that won't help.
unfillable='(load|compile|emit|link|spawn|instrumented compile) failed:'
unfillable+='|\(in-language tests\) failed:|Abort trap|SIGSEGV|SIGABRT'
if grep -qE "$unfillable" "$STEP_OUT"; then
    fail "cargo test (failure a refill can't fix)" "$(grep -nE "$unfillable" "$STEP_OUT" | head -20)"
fi
# Any FAILED test that isn't a case shard (a shard's fillable mismatches are
# handled below) or a known IR-staleness gate is a real test failure.
bad="$(grep -oE 'test [A-Za-z0-9_:]+ \.\.\. FAILED' "$STEP_OUT" | awk '{print $2}' \
       | grep -vE '^(cases_shard_[0-9]+|checked_in_ir_is_current|no_staged_ir_pending)$' \
       || true)"
if [ -n "$bad" ]; then
    fail "cargo test (failing test)" "$bad"
fi

# What (fillable) staleness did we see? A backtick-section mismatch (any of
# stdout / exit code / stderr / errors / check / performance / monomorphizations)
# or an error-fixture mismatch is refilled; the two IR gates trigger an IR regen.
need_fill=0; need_ir=0
grep -qE '`[a-z ]+` mismatch|error mismatch' "$STEP_OUT" && need_fill=1
grep -qE '(checked_in_ir_is_current|no_staged_ir_pending) \.\.\. FAILED' "$STEP_OUT" && need_ir=1
if [ $need_fill -eq 0 ] && [ $need_ir -eq 0 ]; then
    fail "cargo test (unrecognized failure)" "$(tail -40 "$STEP_OUT")"
fi

# Distinct sections that changed (for the review note), and whether any are
# behavioral (worth a closer look at the git diff than a metrics-only refill).
changed_sections="$(grep -oE '`[a-z ]+` mismatch' "$STEP_OUT" | tr -d '`' \
                    | sed 's/ mismatch$//' | sort -u | paste -sd, - | sed 's/,/, /g')"
behavioral_changed=0
grep -qE '`(stdout|stderr|exit code|errors|check)` mismatch|error mismatch' "$STEP_OUT" \
    && behavioral_changed=1

# --- 3. Refill changed sections ------------------------------------------------

if [ $need_fill -eq 1 ]; then
    run_step "fill_expected (section refill — full corpus)" \
        cargo test --test cases -- --ignored fill_expected
    grep -q 'refresh complete' "$STEP_OUT" || { save_out; fail "fill_expected" "$(tail -40 "$STEP_OUT")"; }
fi

# --- 4. Regenerate + validate + promote dogfood IR -----------------------------

if [ $need_ir -eq 1 ]; then
    run_step "fill_staged_ir" cargo test --test dogfood_ir -- --ignored fill_staged_ir
    grep -qE 'wrote .*\.staged' "$STEP_OUT" || { save_out; fail "fill_staged_ir" "$(tail -40 "$STEP_OUT")"; }

    run_step "validate_staged_ir (entry-level pre-check)" \
        cargo test --test dogfood_ir -- --ignored validate_staged_ir
    [ $? -eq 0 ] || { save_out; fail "validate_staged_ir" "$(tail -40 "$STEP_OUT")"; }

    run_step "staged-IR corpus run (AIPL_DOGFOOD_IR)" env AIPL_DOGFOOD_IR="$STAGED" cargo test
    if [ $? -ne 0 ]; then
        save_out
        fail "staged-IR corpus run (candidate IR is wrong — diff .staged vs live)" \
            "$(grep -nE 'mismatch|FAILED|Abort' "$STEP_OUT" | head -20)"
    fi

    run_step "promote_staged_ir" cargo test --test dogfood_ir -- --ignored promote_staged_ir
    grep -q 'promoted' "$STEP_OUT" || { save_out; fail "promote_staged_ir" "$(tail -40 "$STEP_OUT")"; }
fi

# --- 5. Final confirmation against the live artifacts --------------------------

run_step "cargo test (final)" cargo test
if [ $? -ne 0 ]; then
    save_out
    fail "cargo test (final — regeneration didn't settle)" \
        "$(grep -nE 'mismatch|FAILED|Abort' "$STEP_OUT" | head -20)"
fi

# --- Report --------------------------------------------------------------------

printf '\n%sHANDOFF OK%s\n' "$green$bold" "$off" >&2
[ $need_fill -eq 1 ] && printf '  refilled sections: %s\n' "$changed_sections" >&2
[ $need_ir -eq 1 ] && printf '  regenerated + promoted dogfood IR\n' >&2
if [ $behavioral_changed -eq 1 ]; then
    printf '%s  ! behavioral output changed — review the git diff before committing%s\n' \
        "$bold" "$off" >&2
    printf '%s    (git reset --hard HEAD undoes the refill if it recorded a regression)%s\n' \
        "$dim" "$off" >&2
fi
exit 0
