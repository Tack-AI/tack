#!/bin/sh
# Bidirectional self-test for the eval suite — no model, no network needed.
#
# For every task in evals/examples/<name>/ that has a reference solution in
# evals/solutions/<name>.sh, this checks in a throwaway temp dir:
#   1. setup + verify must FAIL on the untouched fixture
#      (the task is not trivially solved / verify is not vacuous)
#   2. setup + reference solution + verify must PASS
#      (the task is solvable and verify accepts the intended outcome)
#
# Usage: evals/selftest.sh [task-filter-substring]
set -u

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
tasks_dir="$here/examples"
solutions_dir="$here/solutions"
filter=${1:-}
failures=0
checked=0

for task_dir in "$tasks_dir"/*/; do
    name=$(basename "$task_dir")
    case "$name" in
        *"$filter"*) ;;
        *) continue ;;
    esac
    task_json="$task_dir/task.json"
    solution="$solutions_dir/$name.sh"
    [ -f "$task_json" ] || continue
    if [ ! -f "$solution" ]; then
        echo "skip $name (no reference solution in evals/solutions/)"
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
        echo "ok   $name (fixture fails verify, reference solution passes)"
    else
        echo "FAIL $name: verify still fails after the reference solution"
        failures=$((failures + 1))
    fi
    rm -rf "$tmp"
done

echo "---"
echo "$checked task(s) checked, $failures failure(s)"
[ "$failures" -eq 0 ]
