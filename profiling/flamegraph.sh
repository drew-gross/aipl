#!/bin/bash
# A flame graph of the compiler's own passes running over its own sources.
#
#     profiling/flamegraph.sh [stage] [file.aipl ...]
#
# `stage` is one of lex, parse, load or format (see `main.aipl`), `load` when
# omitted. With no files it runs over every source of the dogfooded compiler,
# `crates/aipl-codegen/src` and its `lint` folder. The graph is written to
# `target/profiling/<stage>.svg`; open it in a browser.
#
# What it shows is the AIPL half of the compiler. The passes written in Rust —
# flattening imports, the type checker, code generation — are not in it.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
stage="${1:-load}"
[ $# -gt 0 ] && shift

if [ $# -gt 0 ]; then
    files=()
    for file in "$@"; do
        files+=("$(cd "$(dirname "$file")" && pwd)/$(basename "$file")")
    done
else
    files=("$root"/crates/aipl-codegen/src/*.aipl "$root"/crates/aipl-codegen/src/lint/*.aipl)
fi

cargo build -q --manifest-path "$root/Cargo.toml"
mkdir -p "$root/target/profiling"
out="$root/target/profiling/$stage.svg"

# `aipl run` runs the `main.aipl` of its working directory.
cd "$root/profiling"
"$root/target/debug/aipl" run --flamegraph "$out" -- "$stage" "${files[@]}"
echo "flame graph: $out"
