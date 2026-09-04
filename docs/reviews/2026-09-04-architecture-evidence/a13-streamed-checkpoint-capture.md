# Checkpoint capture memory checkpoint

This is a partial A13 implementation. File content capture, Git preimage capture
and workspace fingerprinting now process 64 KiB chunks. Content-addressed blobs
publish without overwriting an existing identity; existing blobs are verified
with bounded reads. Failed reads remove their temporary file. File-version
changes observed during capture or inventory hashing reject that result.

A preimage larger than 64 MiB is refused before a known-path mutation can receive
a checkpoint. Reader limits also cover growth after metadata inspection. A
truncated preimage is never published as a complete blob. Restoring and reviewing
existing blobs enforce their declared size before allocating content.

Nineteen checkpoint tests and all-target `rw-store` clippy pass. New tests use a
sparse oversized file and an instrumented reader to check bounded read buffers,
partial-read cleanup, exact content, deduplication and corrupt existing blobs.

Remaining A13 work: operation/workspace storage quotas, bounded Git inventory
output and inventory metadata, cancellable scans with owned blocking work, and
workspace freshness/reconciliation. This change does not make a large workspace
scan constant-time, prove arbitrary concurrent external writes atomic, or add a
chunk manifest format. The existing whole-file content identity is preserved.
