#!/usr/bin/env bash
# One flamegraph of `aipl check` over the whole repo: every `.aipl` file checked in a
# single process, so the one SVG aggregates all of it — parsing, loading, mono, codegen,
# the JIT'd `.test` blocks and the runs of the compiler's own dogfooded artifact.
#
#     scripts/flamegraph_check.sh                 # tests/cases crates examples tests/ffi_fixtures
#     scripts/flamegraph_check.sh crates/aipl-mono/src tests/cases/lists
#     OUT=/tmp/check.svg INTERVAL_MS=5 scripts/flamegraph_check.sh
#
# Writes `target/flamegraph/check.svg` (or `$OUT`), beside the raw `sample` output
# and the folded stacks it was drawn from. It uses macOS's `sample`, which profiles a
# process you own without sudo, and `inferno` for the drawing (`cargo install inferno`).
#
# The whole repo takes several minutes. `aipl check` exits non-zero here because the
# corpus holds fixtures that fail on purpose, so the exit status is not looked at; the
# run's own output is kept in `check.log` beside the SVG.
set -euo pipefail

cd "$(dirname "$0")/.."

for tool in sample inferno-collapse-sample inferno-flamegraph; do
    command -v "$tool" >/dev/null || {
        echo "missing \`$tool\`$([[ $tool == inferno-* ]] && echo ' — cargo install inferno')" >&2
        exit 1
    }
done

out="${OUT:-target/flamegraph/check.svg}"
dir="$(dirname "$out")"
base="${out%.svg}"
interval="${INTERVAL_MS:-10}"
mkdir -p "$dir"

if (($# > 0)); then
    paths=("$@")
else
    paths=(tests/cases crates examples tests/ffi_fixtures)
fi

cargo build --quiet --package aipl
aipl=target/debug/aipl

echo "checking ${paths[*]} (sampling every ${interval}ms)…" >&2
"$aipl" check "${paths[@]}" >"$base.log" 2>&1 &
pid=$!

# Sample until the process exits. The duration is only a ceiling.
sample "$pid" 86400 "$interval" -mayDie -file "$base.sample.txt" >/dev/null 2>&1 || true
wait "$pid" || true

inferno-collapse-sample <"$base.sample.txt" >"$base.folded"
inferno-flamegraph --title "aipl check ${paths[*]}" --countname samples <"$base.folded" >"$out"
echo "wrote $out" >&2
