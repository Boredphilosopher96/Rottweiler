# Checkpoint ownership and aggregate bounds

The coordinator previously held workspace locks in async callers while blocking workers scanned or wrote files. Dropping the caller could release the lock before those effects stopped. The coordinator now moves its lock into the worker and, when required, into the prepared checkpoint or rewind result. Abandoning an unacknowledged opaque baseline or rewind marks the workspace as requiring recovery. Completion follows the worker's join, including panic; caller cancellation requests interruption without releasing ownership.

Known-path validation failures before tool admission leave valid preimages and do not poison the workspace. Review operations keep their lock in the worker through the final atomic update. Known-path finalization requires no blocking task.

`CheckpointOperation` is a required store argument. A coordinated operation shares one allowance across its workspace roots. It checks cancellation and its deadline between I/O chunks and path visits. It does not claim to interrupt an individual kernel filesystem call that remains blocked.

| Resource | Limit per operation |
| --- | --- |
| Paths visited across inventory, Git queries and capture | 100,000 |
| Aggregate path text | 8 MiB |
| Directory depth | 64 |
| File bytes fingerprinted | 1 GiB |
| Preimage bytes captured | 256 MiB aggregate, 64 MiB per file |
| Capture and hash chunk | 64 KiB |
| Scan/capture deadline | 30 seconds, checked between I/O operations |
| Git query output | 16 MiB per query |
| Persisted manifest/baseline JSON | 32 MiB, bounded during serialization and before parsing |

Git pipes are nonblocking. Cancellation, deadline, or excess output terminates and reaps the owned process group. Missing Git or unsuccessful baseline queries retain explicit incomplete-baseline behavior. Resource exhaustion is an error; it is not converted into a successful checkpoint. Opaque recovery discards each completed manifest after persistence and returns a count instead of retaining every recovered payload.

These are admission bounds, not measured performance guarantees. An oversized workspace is rejected before an opaque tool starts; an interrupted post-scan retains the pending marker for recovery.

## Verification

The complete store suite passed 198 tests with two ignored performance tests, plus the documentation test. Strict store clippy passed for all targets and features. The six focused runtime checkpoint tests also passed, including cancelled-caller ownership, panic poisoning, shared-workspace serialization, multi-root recovery and live-root replacement. Full turn/actor settlement integration and its fallible contract are still being completed. Regression coverage exercises aggregate accounting, depth, cancellation before reads, oversized sparse metadata, blocked Git stdout cancellation and process reaping, excessive Git output, plus existing capture/rewind/recovery behavior.

The semantic extraction separated coordination, path resolution and recovery. Its pre-change and post-change Rust function-token multisets match. All new checkpoint modules are below 1,500 lines; the remaining runtime entrypoint still needs its broader split.
