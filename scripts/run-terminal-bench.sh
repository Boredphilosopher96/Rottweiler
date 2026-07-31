#!/bin/bash
set -euo pipefail

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tasks="$repo/evals/terminal-bench-20.txt"
: "${ROTTWEILER_RELEASE_ARCHIVE:?set to a Linux release archive from scripts/build-release.sh}"
: "${ROTTWEILER_EVAL_MODEL:?set to a pinned provider/model identifier}"
: "${ROTTWEILER_EVAL_OUTPUT_DIR:?set to a dedicated Harbor evidence directory}"
: "${ROTTWEILER_EVAL_API_KEY:?set to the job-scoped model credential}"

command -v harbor >/dev/null
command -v docker >/dev/null
test -f "$ROTTWEILER_RELEASE_ARCHIVE"
test "$(grep -c '^terminal-bench/[a-z0-9-][a-z0-9-]*$' "$tasks")" -eq 20
case "$ROTTWEILER_EVAL_MODEL" in
  *-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]|*-[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]|*@[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]) ;;
  *) echo 'ROTTWEILER_EVAL_MODEL must use an immutable dated model id' >&2; exit 1 ;;
esac

case "$ROTTWEILER_EVAL_MODEL" in
  github/*@*)
    model_id=${ROTTWEILER_EVAL_MODEL#github/}
    expected_version=${model_id##*@}
    model_id=${model_id%@*}
    catalog=$(curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
      -H "Accept: application/vnd.github+json" \
      -H "Authorization: Bearer $ROTTWEILER_EVAL_API_KEY" \
      -H "X-GitHub-Api-Version: 2026-03-10" \
      https://models.github.ai/catalog/models)
    MODEL_ID="$model_id" EXPECTED_VERSION="$expected_version" python3 -c '
import json
import os
import sys

models = {item["id"]: item for item in json.load(sys.stdin)}
model = models.get(os.environ["MODEL_ID"])
if model is None:
    raise SystemExit("pinned GitHub model is absent from the catalog")
if model.get("version") != os.environ["EXPECTED_VERSION"]:
    raise SystemExit("GitHub model catalog version changed")
' <<<"$catalog"
    ;;
  *)
    echo 'ROTTWEILER_EVAL_MODEL must select a version-pinned GitHub Models entry' >&2
    exit 1
    ;;
esac

harbor_version=$(harbor --version 2>&1)
case "$harbor_version" in
  *0.18.0*) ;;
  *) echo "expected Harbor 0.18.0, found: $harbor_version" >&2; exit 1 ;;
esac
git_commit=$(git -C "$repo" rev-parse HEAD)
if [ -n "${GITHUB_SHA:-}" ]; then
  [ "$git_commit" = "$GITHUB_SHA" ] || {
    echo 'checked-out commit does not match GITHUB_SHA' >&2
    exit 1
  }
fi
archive_sha256=$(python3 - "$ROTTWEILER_RELEASE_ARCHIVE" <<'PY'
import hashlib
import sys
from pathlib import Path

digest = hashlib.sha256()
with Path(sys.argv[1]).open("rb") as source:
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(chunk)
print(digest.hexdigest())
PY
)

mkdir -p "$ROTTWEILER_EVAL_OUTPUT_DIR"
[ -z "$(find "$ROTTWEILER_EVAL_OUTPUT_DIR" -mindepth 1 -print -quit)" ] || {
  echo 'ROTTWEILER_EVAL_OUTPUT_DIR must be empty' >&2
  exit 1
}

includes=()
while IFS= read -r task; do
  includes+=(--include-task-name "$task")
done < "$tasks"

export PYTHONPATH="$repo${PYTHONPATH:+:$PYTHONPATH}"
(
  cd "$ROTTWEILER_EVAL_OUTPUT_DIR"
  harbor run \
    --dataset terminal-bench/terminal-bench-2-1@6 \
    --agent evals.harbor.rottweiler_agent:Rottweiler \
    --model "$ROTTWEILER_EVAL_MODEL" \
    --env docker \
    --n-concurrent "${ROTTWEILER_EVAL_CONCURRENCY:-2}" \
    "${includes[@]}" 2>&1 | tee harbor.log
)

MODEL="$ROTTWEILER_EVAL_MODEL" GIT_COMMIT="$git_commit" ARCHIVE_SHA256="$archive_sha256" \
  python3 - "$ROTTWEILER_EVAL_OUTPUT_DIR/rottweiler-eval-manifest.json" <<'PY'
import json
import os
import sys
from pathlib import Path

Path(sys.argv[1]).write_text(json.dumps({
    "dataset": "terminal-bench/terminal-bench-2-1@6",
    "git_commit": os.environ["GIT_COMMIT"],
    "harbor_version": "0.18.0",
    "model": os.environ["MODEL"],
    "release_archive_sha256": os.environ["ARCHIVE_SHA256"],
    "task_count": 20,
}, sort_keys=True) + "\n", encoding="utf-8")
PY
