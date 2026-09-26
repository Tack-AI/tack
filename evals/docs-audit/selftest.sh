#!/bin/sh
# Bidirectional self-test for the docs-audit agent tasks — no model, no
# network needed. Mirrors evals/selftest.sh:
#   1. setup + verify must FAIL on the untouched fixture
#      (verify is not vacuous: there is no audit-report.json yet)
#   2. setup + reference solution + verify must PASS
#      (the task is solvable and verify accepts the intended outcome)
#
# setup copies live doc/source snapshots out of the repo, so the expected
# verdict is recomputed on every run — this self-test passes whether or not
# the docs currently drift (the reference solution derives the same verdict).
#
# Usage: evals/docs-audit/selftest.sh [task-filter-substring]
set -u

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
# The tasks' setup locates the repo via TACK_ROOT (default ../../.. works
# when `tack eval` runs them in-place; under this self-test they run from a
# throwaway temp dir, so point them back at the real repo).
TACK_ROOT=$(CDPATH= cd -- "$here/../.." && pwd)
export TACK_ROOT
solutions_dir="$here/solutions"
filter=${1:-}
failures=0
checked=0

for task_dir in "$here"/*/; do
    name=$(basename "$task_dir")
    case "$name" in
        solutions) continue ;;
        *"$filter"*) ;;
        *) continue ;;
    esac
    task_json="$task_dir/task.json"
    solution="$solutions_dir/$name.sh"
    [ -f "$task_json" ] || continue
    if [ ! -f "$solution" ]; then
        echo "skip $name (no reference solution in evals/docs-audit/solutions/)"
        continue
    fi
    checked=$((checked + 1))
    tmp=$(mktemp -d)
    cp -R "$task_dir"/. "$tmp"/
    setup=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1])).get("setup") or "")' "$task_json")
    verify=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["verify"])' "$task_json")

    if ! (cd "$tmp" && { [ -z "$setup" ] || bash -c "$setup"; }) > /dev/null 2>&1; then
        echo "FAIL $name: setup command failed"
        failures=$((failures + 1))
    elif (cd "$tmp" && bash -c "$verify") > /dev/null 2>&1; then
        echo "FAIL $name: verify passes on the unsolved fixture"
        failures=$((failures + 1))
    elif (cd "$tmp" && bash "$solution" > /dev/null 2>&1 && bash -c "$verify") > /dev/null 2>&1; then
        expected=$(cat "$tmp/.expected" 2>/dev/null || echo '?')
        echo "ok   $name (fixture fails verify, reference solution passes; current verdict: $expected)"
    else
        echo "FAIL $name: verify still fails after the reference solution"
        failures=$((failures + 1))
    fi
    rm -rf "$tmp"
done

echo "---"
echo "$checked task(s) checked, $failures failure(s)"
[ "$failures" -eq 0 ]
