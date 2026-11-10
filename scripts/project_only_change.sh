#!/usr/bin/env bash
# Is the working tree's whole change inside one project directory?
#
# Prints the project's name and exits 0 when every changed path (staged, unstaged or untracked) is
# under `projects/<name>/` for a single <name>, apart from the TODO texts (TODO.txt,
# CLAUDE_TODO.txt), which never count. Prints what is outside and exits 1 otherwise; exits 2 when
# nothing but TODO texts (or nothing) changed. Reads git, never writes it.
#
# Such a change is finished with `./aipl check` in the project directory, not `cargo handoff`.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

changed=$( { git diff --name-only HEAD; git ls-files --others --exclude-standard; } | sort -u )
projects=""
outside=""
while IFS= read -r path; do
    [ -z "$path" ] && continue
    case "$path" in
        TODO.txt | CLAUDE_TODO.txt) ;;
        projects/*/*) projects="$projects"$'\n'"$(echo "$path" | cut -d/ -f2)" ;;
        *) outside="$outside"$'\n'"$path" ;;
    esac
done <<< "$changed"

if [ -n "$outside" ]; then
    echo "not project-only; changed outside a project:$outside" >&2
    exit 1
fi
names=$(echo "$projects" | sort -u | sed '/^$/d')
case $(echo "$names" | sed '/^$/d' | wc -l | tr -d ' ') in
    0) echo "nothing but TODO texts changed" >&2; exit 2 ;;
    1) echo "$names" ;;
    *) echo "more than one project changed:" >&2; echo "$names" >&2; exit 1 ;;
esac
