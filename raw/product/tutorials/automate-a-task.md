This tutorial turns a repository review into a script-friendly operation.

## Run one bounded prompt

```sh
rw -p "Review the staged patch. Report correctness issues before style issues." \
  --permission-mode strict \
  --max-turns 12 \
  --output-format json
```

The three permission modes are `strict`, `auto-safe`, and `yolo`. `yolo`
removes interactive approvals; it does not remove workspace, trust, or platform
sandbox boundaries.

## Stream events

Use streaming JSON when a caller needs progress instead of one final document:

```sh
rw -p "Run the focused tests and explain any failure" \
  --permission-mode auto-safe \
  --max-turns 16 \
  --output-format stream-json > run.jsonl
```

Each line is independently parseable JSON. Treat the stream as an event log,
not terminal prose.

## Add piped context

Standard input can carry a bounded artifact alongside the prompt:

```sh
git diff --staged | rw -p "Review this patch for regressions" \
  --permission-mode strict \
  --max-turns 8
```

Do not use `yolo` merely to avoid designing an approval policy. Prefer the
least-powerful mode that can complete the operation.
