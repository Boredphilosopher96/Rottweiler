# Durable workflow state

The existing workflow runner now requires a journal. A run has a random 128-bit identity and every node has a task identity consisting of that run and its step name. The stored definition hash, parent session and exact task set must match before reopening. A separate exclusive lock file remains owned across atomic snapshot replacement.

A wave is claimed durably before any executor starts. Child identity is bound before the first child turn. Completed receipts are persisted after child cleanup; a resumed runner reuses those outcomes as dependency inputs. A started node without a terminal receipt is an unresolved obligation and cannot execute again. Failed stop nodes stay failed on replay. Parallel peers with proven outcomes retain their receipts even when another peer is unresolved or too large to persist.

The snapshot supports at most 64 steps, 256 dependency edges, 256 KiB per outcome and 1 MiB aggregate outcomes. Serialized state is bounded at 2 MiB before writing and reading. Immutable artifact payloads are shared across snapshots, reports and parallel requests. File replacement and directory entries are synchronized before a transition acknowledges. A read-only snapshot can inspect an active writer, with parent identity checked before returning state.

`/workflow <name> [run-id]` creates or resumes a run. `/workflow-status <run-id>` reads its current task states. Started means the terminal receipt is absent; it does not assert that a process is still alive. Restart can resume work between completed waves. Interruption during a claimed task requires reconciliation and never authorizes blind repetition.

## Ownership work in progress

The whole workflow invocation and child cleanup outlive a dropped tool caller. Cleanup failures retain bounded admission and resource obligations and return an explicit unsettled error. The shared tool/model settlement contracts are being migrated so failures reach turn and shutdown barriers without an indefinite completion wait.

Child startup has a bounded owner and reply acknowledgement. Thirty-two integrated orchestration tests now pass, including pre-turn cancellation, factory panic, failed cleanup and production worktree factory rejection. Failure before the first turn closes the created session. An uncertain Spawned acknowledgement requires a Closed recovery receipt so recovery can inspect the parent journal without reattaching a deleted worktree. Confirmed Spawned requires an acknowledged terminal before metadata removal. Closed recovery adoption remains to qualify with runtime shutdown.

A worktree allocation remains provisional until its child session is constructed. The factory must either transfer its lease or prove rollback. Changed/locked worktrees and abandoned allocations are preserved and block further creation through that isolation owner. Factory panics and explicit unsettled errors retain startup admission even when no child session was returned. Partial checkout cleanup verifies both Git registration and directory removal, and rejects symlink replacement.

## Verified evidence and limits

`cargo test --locked -p rw-store -p rw-ext -p rw-runtime --lib workflow` passed 8 extension tests, 11 runtime tests and 4 store tests before the fallible shared settlement migration. Runtime coverage includes actual actor/worktree execution and 257 repeated workflows without retained child metadata. Store coverage includes active-writer status reads, foreign-parent rejection, atomic wave admission, reopen with ambiguous tasks, exact terminal retry and oversized outcomes. Strict all-target/all-feature clippy passed for those three crates before the startup/settlement changes.

The workflow store subsequently passed five tests including a subprocess exit that skips Rust destructors, followed by reopening the durable Started receipt. At root4f33187, all119 tools tests and strict all-target/all-feature tools Clippy passed. Thirty worktree tests include six new provisional-allocation cases: rollback, abandonment, changed files, locked registration, partial checkout removal and symlink replacement.

After actor-close integration at af06721, all32 core orchestration tests pass, including the startup owner and production factory rollback tests. All9 extension workflow tests also pass with required fallible settlement, including retained receipts beside an oversized parallel peer. The registered status command and complete runtime resource shutdown integration remain to qualify. Process-kill recovery, Closed receipt adoption and full actor control-latency integration remain required. No long-soak or performance qualification is claimed by these unit/integration results.
