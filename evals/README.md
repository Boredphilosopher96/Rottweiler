# Capability evaluation

Rottweiler's v1.0 capability gate uses an explicit 20-task subset of the
official Terminal-Bench 2.1 Harbor dataset, pinned to registry revision 6. The
subset favors coding, debugging, data recovery, and systems tasks and avoids
GPU-only work so harness overhead can be compared consistently.

Build a Linux release archive, install Harbor, start Docker, provide a pinned
model and its provider credential, then run:

```sh
ROTTWEILER_RELEASE_ARCHIVE=/path/to/rottweiler-linux.tar.gz \
ROTTWEILER_EVAL_MODEL=openai/gpt-5-mini \
scripts/run-terminal-bench.sh
```

The adapter uploads the exact local archive, verifies its SHA-256 inside each
task container, and runs the normal headless `rw` entrypoint. Provider secrets
are transferred through a private temporary file, sourced only for the agent
process, deleted before the turn begins, and never placed in a command line or
Rottweiler event log. Harbor retains verifier reward, Rottweiler stream JSON,
and `rw stats --json` output for solve-rate, token, wall-time, and cost analysis.

Live benchmark execution is intentionally restricted to the nightly/release
networked lane. Pull-request tests exercise the same CLI using replay fixtures.
