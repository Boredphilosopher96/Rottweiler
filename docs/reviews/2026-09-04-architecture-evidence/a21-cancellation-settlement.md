# A21 ordinary native request cancellation

The host now closes a shared native plugin when an admitted ordinary RPC times out, receives cancellation, or loses its caller future. It revokes new host requests, cancels owned HTTP futures, drains admitted actor pushes, kills and reaps the native process, and waits for the launcher-owned process group to have no live members. Engine tool and hook barriers remain outside cancellable invocation futures so the two-second cleanup grace cannot finalize a checkpoint over pending teardown.

## Design choice

A cooperative per-request cancellation RPC would preserve sibling requests and avoid process restart, but an acknowledgement cannot stop arbitrary native code or prove its children have stopped. Shared-process termination gives one owner for cancellation and is fail-closed: sibling calls fail, and new admission requires a fresh plugin instance. The existing provider-stream cancellation wire remains distinct.

Drop starts one idempotent host-owned teardown task. Its completion includes the supervisor's settlement proof and all admitted host-effect permits. Sending SIGKILL alone never releases the barrier. A kill/reap inspection error leaves the operation pending and logs a diagnostic. The existing process-group helper distinguishes Darwin EPERM with an independent process-state inspection, including zombie-only groups. Capability-violation diagnostics remain separate from the direct-child exit observation, so a recorded denial does not falsely mean a reaped child is still running.

The host push handler drains the reply from an already-enqueued actor command. Cancelling its future would only discard the reply while the actor kept executing, so the reader retains that future and its effect permit. The production HTTP adapter owns its guarded request and response-body futures; cancellation drops those local resources. Cancellation does not roll back requests already accepted by a remote server.

## Reproducible checks

Run from the repository root:

```sh
cargo test -p rw-ext --lib
cargo test -p rw-core dropped_tool_future_keeps_checkpoint_open_until_external_effects_settle --lib
cargo test -p rw-core cleanup --lib
cargo clippy -p rw-ext -p rw-runtime -p rw-core -p rw-tools --all-targets -- -D warnings
```

Observed on the macOS development host: all 115 rw-ext tests passed; the external-tool settlement test passed; both engine cleanup tests passed; scoped all-target clippy passed.

The real process regressions launch `/bin/sh` with a parent and child continually appending separate files. Cancellation, timeout, and dropped hook futures must settle before a subsequent write replaces both files; neither plugin process may append afterward. These use the direct test launcher and prove process lifecycle, not sandbox enforcement.

The actor-push regression delegates a delayed mutation to an independent task and blocks its completion with a gate. Ordinary cancellation cannot return before that task commits and replies. The HTTP regression ignores its cancellation token and verifies the owning request future is dropped before cancellation returns. The engine regression lets the invocation exceed its two-second grace, then proves the checkpoint and terminal events remain blocked until the external cleanup gate completes.

## Remaining containment limit

The native macOS supervisor owns a Unix process group. A deliberately detached `setsid` descendant can escape that group; this change does not supply a kernel-enforced macOS descendant boundary. Linux already uses a PID namespace and has a dedicated setsid-descendant sandbox test, but no new Linux execution was performed for this patch. A21's ordinary-process and host-effect ordering is repaired; adversarial escaped-descendant qualification on macOS remains open. Do not call the group inspection proof of arbitrary native descendant containment.
