# Foreground Bash cancellation settlement

The engine's two-second cancellation grace can drop a tool invocation before its cleanup future finishes. The Bash watchdog owns a crash-recovery lease, but that lease does not stop the same live engine from finalizing its current checkpoint. Bash previously inherited the tool registry's no-op settlement method.

## Reproduction

`foreground_cleanup_and_recording_survive_caller_drop` launches a real shell through a command executor whose native cleanup is held at a deterministic gate. It cancels the caller, exhausts the two-second grace, drops the caller future, and checks that settlement still blocks. On the parent checkpoint `a1f4f98`, the new regression failed after 2.03 seconds:

```text
Bash released its settlement barrier while native cleanup was still pending
```

After releasing cleanup, the test requires the cancelled command recording to exist and verifies a subsequent conflicting write remains unchanged. The gate models delayed or unprovable cleanup; it does not claim an ordinary SIGKILL takes two seconds on this host.

## Ownership change

Bash owns each foreground executor invocation in a task. A caller-drop guard cancels that invocation and marks it abandoned. The engine's existing `Tool::settle_effects` barrier waits for abandoned invocations before finishing the checkpoint. A new foreground invocation also waits for prior abandoned cleanup. Active supervised background commands have separate ownership and are excluded from this foreground registry.

The owned task encloses the complete executor chain. Owning only `TokioCommandExecutor` would still let dropped callers abandon recording middleware and scratch ownership. Completed entries remove themselves immediately; cleanup does not depend on another call arriving. An owned-task panic closes its completion channel without proof and leaves settlement pending with a diagnostic.

Native cleanup shares one kill, direct-child reap, and process-group inspection path. An inspection or reap error remains pending instead of returning an error that callers could mistake for completed cleanup. Watchdog errors and stdout errors no longer skip the other output task. Both output tasks are drained concurrently; a timed-out reader is aborted and joined before cleanup completes.

## Verification

Run from this checkout:

```sh
cargo fmt --all --check
cargo test -p rw-tools
cargo clippy -p rw-tools --all-targets -- -D warnings
```

All checks passed on the macOS development host. The tools suite passed 110 tests. Besides the grace/drop/recording regression, a real-process test drops a foreground call without first cancelling its token, then verifies both shell parent and descendant stop before subsequent writes and that the watchdog releases the execution lease. An output regression proves stdout failure still waits for stderr. Existing process-group cancellation, watchdog death, crash-recovery lease, background supervision, and command recording tests also passed.

The Linux-specific test executables were built but their Linux behavior was not exercised on macOS. No native Linux performance or containment qualification is claimed. This patch preserves the existing process-group and sandbox boundaries; it does not add containment for deliberately escaped macOS descendants. Background-manager shutdown deadlines remain a separate lifecycle path.
