# Protected queue and diagnostics checkpoint

September 4, 2026. Local source validation only.

The nightly workflow dispatches one source-qualified worker per platform.
Workers validate the producer attempt and immutable bundle identities before
native execution. A hosted watcher bounds the native queue to fifteen minutes,
retains its result, then requests cancellation of its own worker if no start was
observed. Bootstrap, observation, upload and cancellation have separate deadlines.

Validation passed:

- All 154 Python contract tests, including eleven dispatcher/queue tests and
  bundle mismatch, symlink, unexpected-file and bounded-manifest checks.
- actionlint 1.7.12 over every workflow, package inventory, ownership and docs checks.
- Existing full Rust workspace/all-feature tests, all-target/all-feature clippy,
  formatting, both protocol generators and locked compilation of every fuzz bin.
  The queue changes do not modify Rust source.

The queue tests exercise failed-job reruns, latest-producer selection, expired
artifacts, API budget exhaustion, lost dispatch responses and owned cancellation.
Bundle manifests identify every file by content digest, size, source and platform.
Soak progress emits only metadata to the job log at most once per minute.

`gh` reports zero registered repository runners. No eight-hour workload or hosted
queue fault injection ran. Accepted cancellation is not confirmed terminal
cancellation. Hosted watcher/API availability, abrupt runner-loss retention,
v1 release integration and cross-platform qualification remain open.
