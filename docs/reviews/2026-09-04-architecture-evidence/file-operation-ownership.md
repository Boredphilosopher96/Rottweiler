# File-operation ownership

Commit adda821 moves read, write, edit, multi-edit and approval-preview filesystem work into owned blocking transactions. The caller no longer owns a chain of cancellable Tokio filesystem futures.

Each tool and its clones share a maximum of 16 admitted transactions. A caller drop cancels its context and marks its operation abandoned. The completion owner joins the actual blocking worker before publishing settlement proof. Successful replies keep their admission credit until consumed or dropped. Settlement checks abandoned operations without waiting for unrelated live callers.

The settlement wait is bounded at 30 seconds. A blocked worker remains owned after that deadline. Failed temporary-file rollback closes admission and retains the transaction, pinned parent descriptor, workspace context and permit. A panic in the file operation still runs rollback before its error is returned. This per-tool limit does not establish an aggregate application resource bound.

Synchronous reads use bounded 16 KiB chunks and writes check cancellation between 64 KiB chunks. The Unix path keeps descriptor-pinned workspace traversal, no-follow opens, regular-file checks, preserved permissions, snapshot identity/hash checks, atomic rename and parent-directory synchronization. Cancellation cannot interrupt a kernel syscall; the ownership barrier accounts for that.

Validation on macOS ARM64:

- All 124 rw-tools unit tests passed.
- All 15 file-tool tests passed again after the final import and compare-and-swap error cleanup.
- Strict all-target, all-feature rw-tools Clippy passed.
- Five new ownership tests cover normal reply cancellation behavior, abandoned blocked workers, unconsumed result admission, panic rollback and failed rollback quarantine.
- Existing tests still cover escaping links, special files, added workspace roots, size limits, atomic edit batches, executable permissions and symbol-index updates.

The largest resulting file is files.rs at 639 lines. IO, transaction state, operation ownership and tests have separate modules. Native Linux execution remains part of integrated CI qualification.
