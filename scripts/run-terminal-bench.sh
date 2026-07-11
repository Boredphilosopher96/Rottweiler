#!/bin/bash
set -euo pipefail

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tasks="$repo/evals/terminal-bench-20.txt"
: "${ROTTWEILER_RELEASE_ARCHIVE:?set to a Linux release archive from scripts/build-release.sh}"
: "${ROTTWEILER_EVAL_MODEL:?set to a pinned provider/model identifier}"

command -v harbor >/dev/null
command -v docker >/dev/null
test -f "$ROTTWEILER_RELEASE_ARCHIVE"
test "$(grep -c '^terminal-bench/[a-z0-9-][a-z0-9-]*$' "$tasks")" -eq 20

includes=()
while IFS= read -r task; do
  includes+=(--include-task-name "$task")
done < "$tasks"

export PYTHONPATH="$repo${PYTHONPATH:+:$PYTHONPATH}"
harbor run \
  --dataset terminal-bench/terminal-bench-2-1@6 \
  --agent evals.harbor.rottweiler_agent:Rottweiler \
  --model "$ROTTWEILER_EVAL_MODEL" \
  --env docker \
  --n-concurrent "${ROTTWEILER_EVAL_CONCURRENCY:-2}" \
  "${includes[@]}"
