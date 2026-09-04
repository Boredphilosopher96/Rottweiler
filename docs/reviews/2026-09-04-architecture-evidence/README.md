# Architecture review evidence

These probes document findings in the adjacent mission architecture review at
`c729e3bf4d87b6c80f6a2e8655ed935aeb67cafd`. They are diagnostic evidence, not product
changes or a substitute for integration regressions. Exit zero means the stated
failure was reproduced. After fixing a finding, change its assertion to require
correct behavior.

Run from the repository root:

```sh
bun run docs/reviews/2026-09-04-architecture-evidence/malformed-event-probe.ts
bun run docs/reviews/2026-09-04-architecture-evidence/plugin-duplex-probe.ts
CARGO_TARGET_DIR=/tmp/rw-ordered-output-probe-target cargo run --locked --offline --quiet --manifest-path docs/reviews/2026-09-04-architecture-evidence/ordered-output-probe/Cargo.toml
```

The malformed-event probe directly invokes production normalization and reduction.
It demonstrates a TypeError from a malformed known event; it does not exercise
HTTP reconnection. The duplex probe uses production SDK dispatch with an
in-memory transport and a shortened timeout. The Rust probe extracts the
production output coordinator and replaces surrounding dependencies with stubs.
Read the individual probe notes for their limits.

All three were reproduced locally. Bun was 1.4.0, while the repository pins 1.3.14.
The standalone Cargo lockfile records the dependencies used for the Rust probe.
No provider credentials, network calls, or user-session data are used by these
probes. Offline Cargo requires the locked dependencies to be cached.

## CI reliability follow-up

`ci-live-snapshot.json` records the September 4 read-only GitHub API inventory:
recent ordinary CI runs, current-main job results, the active ruleset, runner
availability, and selected hardening runs. This is a point-in-time snapshot,
not a claim that these settings or statuses remain unchanged. Job links in the
report identify the original logs. No workflows were rerun or hosted settings
changed. The historical soak summary preserves only its failure phase and
artifact metadata, not raw terminal output.

Run the additional parser diagnostic from the repository root:

```sh
python3 docs/reviews/2026-09-04-architecture-evidence/scaffold-crlf-probe.py
```

It copies the unchanged production scaffold source and canonical fixtures into
a temporary directory, compares LF input with CRLF input, and verifies the
malformed destination names. It needs Bun but installs nothing. Exit zero means
the defect was reproduced. The integrating reviewer ran it with Bun 1.4.0; this
is parser evidence, not full WSL qualification. The archived WSL checkout bytes
were unavailable, so the matching hosted failure remains a strongly supported
attribution rather than an exact environment reproduction.
