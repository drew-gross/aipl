#!/usr/bin/env bash
# A flamegraph of the language server answering go-to-definition on a freshly opened file:
# it starts `aipl lsp`, sends `didOpen` and then `textDocument/definition`, and samples the
# server until the answer arrives. What it draws is what an editor waits for after opening a
# file and jumping to a name.
#
#     scripts/flamegraph_lsp.sh crates/aipl-codegen/src/walker.aipl 'fmt_kind(t.kind)'
#     OUT=/tmp/lsp.svg INTERVAL_MS=5 scripts/flamegraph_lsp.sh file.aipl 'name at a use'
#
# The second argument is text that occurs in the file at a name the compiler resolves; the
# request is made at its start. Writes `target/flamegraph/lsp.svg` (or `$OUT`) and the
# inverted graph beside it (`lsp-inverted.svg`: leaf functions at the bottom, which shows what
# is worth making faster), with the raw `sample` output and the folded stacks they came from.
# It uses macOS's `sample`, which profiles a process you own without sudo, and `inferno`
# for the drawing (`cargo install inferno`). The time to the answer is printed.
set -euo pipefail

cd "$(dirname "$0")/.."

(($# == 2)) || {
    echo "usage: $0 <file.aipl> <text at the name to look up>" >&2
    exit 1
}
for tool in sample python3 inferno-collapse-sample inferno-flamegraph; do
    command -v "$tool" >/dev/null || {
        echo "missing \`$tool\`$([[ $tool == inferno-* ]] && echo ' — cargo install inferno')" >&2
        exit 1
    }
done

out="${OUT:-target/flamegraph/lsp.svg}"
base="${out%.svg}"
interval="${INTERVAL_MS:-10}"
mkdir -p "$(dirname "$out")"

cargo build --quiet --package aipl

python3 - target/debug/aipl "$1" "$2" "$base.sample.txt" "$interval" <<'PYTHON'
import json
import os
import subprocess
import sys
import time

binary, path, snippet, sampled, interval = sys.argv[1:6]
path = os.path.realpath(path)
text = open(path).read()
offset = text.index(snippet)
line = text.count("\n", 0, offset)
character = offset - (text.rfind("\n", 0, offset) + 1)


def frame(message):
    body = json.dumps(message).encode()
    return b"Content-Length: %d\r\n\r\n" % len(body) + body


def read_message(stream):
    length = None
    while True:
        header = stream.readline().strip()
        if not header:
            if length is None:
                return None
            break
        if header.lower().startswith(b"content-length:"):
            length = int(header.split(b":")[1])
    return json.loads(stream.read(length))


server = subprocess.Popen(
    [binary, "lsp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL
)
uri = "file://" + path
server.stdin.write(
    frame(
        {
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {"rootUri": "file://" + os.path.dirname(path)},
        }
    )
)
server.stdin.flush()
read_message(server.stdout)

# The duration is a ceiling: the sampler is let go once the answer is in.
sampler = subprocess.Popen(
    ["sample", str(server.pid), "600", interval, "-mayDie", "-file", sampled],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
time.sleep(1)  # the sampler has to attach before the work starts

start = time.time()
server.stdin.write(
    frame(
        {
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "aipl", "version": 1, "text": text}},
        }
    )
)
server.stdin.write(
    frame(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "textDocument/definition",
            "params": {"textDocument": {"uri": uri}, "position": {"line": line, "character": character}},
        }
    )
)
server.stdin.flush()
while True:
    message = read_message(server.stdout)
    if message is None or message.get("id") == 1:
        break
print(f"definition answered in {time.time() - start:.2f}s", file=sys.stderr)
if message is None or "error" in message:
    print(f"no answer to look up: {message}", file=sys.stderr)
server.kill()
sampler.wait()
PYTHON

inferno-collapse-sample <"$base.sample.txt" >"$base.folded"
title="aipl lsp: open + definition of $(basename "$1")"
inferno-flamegraph --title "$title" --countname samples <"$base.folded" >"$out"
inferno-flamegraph --inverted --title "$title (inverted)" --countname samples \
    <"$base.folded" >"$base-inverted.svg"
echo "wrote $out and $base-inverted.svg" >&2
