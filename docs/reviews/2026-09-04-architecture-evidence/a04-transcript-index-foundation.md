# A04 transcript index foundation

Status: isolated storage foundation; no production transcript caller or wire migration yet. This does not close A04. ADR-030 and `docs/design/paged-transcript-client.md` define the accepted current-effective-transcript contract.

## Persistence decision

The existing SQLite history helpers copy the database into a private directory before reading. Reusing that path would make opening history proportional to index size. A descriptor-path SQLite prototype passed macOS rename/link tests but failed the portability requirement: Linux SQLite canonicalized `/dev/fd/<file>` back into an ordinary path. That prototype was discarded before commit.

The replacement uses pinned `redb = 4.2.0`. Its `FileBackend` takes the already opened file and performs positioned reads/writes against it. Rottweiler opens the derived directory and regular single-link database with descriptor-relative `NOFOLLOW`, holds an independent index lock, and bounds the database cache to 4 MiB and file extent to 4 GiB. The raw journal writer continues independently. Index batches reject a read view from a different derived-directory identity, including journals with identical empty or nonempty prefixes.

The crate's MSRV is 1.90; this workspace uses 1.97. The lock delta adds only redb. A fresh advisory audit found no redb advisory. The complete older branch audit separately reports the already existing Wasmtime advisories and a yanked chacha20 version; those are not introduced by this dependency and parent integration owns the current dependency baseline.

Primary references: [redb 4.2.0 descriptor API](https://docs.rs/redb/4.2.0/redb/struct.Builder.html), [file backend source](https://github.com/cberner/redb/blob/v4.2.0/src/tree_store/page_store/file_backend/optimized.rs), [transaction/storage design](https://github.com/cberner/redb/blob/v4.2.0/docs/design.md).

## Implemented contracts

Core supplies opaque semantic rows and checkpoints. The store indexes dense ordinals, stable item identity and owning turn. Row revision and source event identities are retained separately. Updates cannot change identity/order or change content without advancing its revision. Head metadata and every row/index mutation commit in one transaction. Failed batches publish neither partial rows nor a later checkpoint.

Each batch admits at most 128 mutations and 1 MiB of charged row data. Checkpoints have a separate 64 KiB cap. Preview payloads stop at 32 KiB minus 512 bytes so row metadata fits below redb's next page-allocation tier. Moving a row charges the maximum bounded payload; delete charges fixed metadata. Pages admit at most 64 rows and 1 MiB of charged result data, with explicit errors when even one row cannot fit. These are storage allocation contracts, not measured whole-process RSS bounds.

Rebuild batches can temporarily contain ordinal holes, but ordinary page reads refuse that state. Publishing a complete generation checks first/last ordinals against the table's constant-time length. The index never scans or counts all preceding rows for a normal page or source-key lookup. Full automatic database repair is disabled at open; damaged or incompatible derived state requires an explicit reset/rebuild. Raw journal authority remains intact.

Normal redb release opens load allocator metadata. Its debug build additionally walks allocated pages, so production I/O qualification explicitly requires `cargo test --release`; debug whole-index traversal is not presented as release behavior. No existing benchmark, budget or statistic changed.

## Verification

The focused tests exercise real segmented journals and the actual index API:

- 10K first/middle/latest reads and reopening, with every expected source row asserted.
- Later row revisions, unchanged already materialized pages, stale-prefix rejection and unchanged-revision content rejection.
- Atomic rollback after a partially processed invalid batch.
- A separate process exiting during an uncommitted index transaction.
- Damaged derived data followed by explicit rebuild, with authoritative journal verification.
- Directory/file rename, symlink and hard-link rejection, independent index ownership and continued raw writes.
- Identical-prefix cross-session rejection before any index mutation.
- Maximum-size previews across bounded batches/pages, complete byte-for-byte reachability, per-batch write bounds and rejection before writes when aggregate bytes exceed the limit.

The full `rw-store` suite passed: 157 tests, 2 ignored. Strict clippy passed. The actual cross-compiled Linux test binary passed all 10 focused index tests in an amd64 Linux container, including the separate-process interruption and cross-session cases. The release qualification passed separately. All builds use this worktree's private target directory.

| Fixture | Explicit build | Cold index bytes read | 64-row page bytes read | One revised row bytes written |
| --- | ---: | ---: | ---: | ---: |
| 10K rows | 0.552 s | 29,001 | 8,192–12,288 | 28,992 |
| 100K rows | 4.927 s | 29,001 | 8,192–16,384 | 28,992 |

Each measured single-row revision performed one index sync. Page times ranged from 0.013 to 0.071 ms; cold opens from 4.36 to 7.53 ms. These are six local samples, not p95 claims or hosted release-gate evidence. Backend counters cover the index; raw prefix validation is separate A02 work. Maximum-body tests also assert less than 4 MiB written per bounded batch and verify all 4,128,768 payload bytes remain reachable across result pages.

The [retained qualification samples](a04-index-qualification.jsonl) count actual backend reads/writes rather than inferring work from returned page length. Their rows are synthetic opaque previews, not yet the production mixed semantic transcript fixture.

Repeat on macOS or Linux:

```sh
CARGO_TARGET_DIR=/tmp/rw-client-bounds/target cargo test -p rw-store --lib
CARGO_TARGET_DIR=/tmp/rw-client-bounds/target cargo clippy -p rw-store --lib --tests -- -D warnings
CARGO_TARGET_DIR=/tmp/rw-client-bounds/target cargo test --release -p rw-store qualify_10k_100k_index_work --lib -- --ignored --nocapture
```

Local full logs: `/tmp/rw-a04-store-all-tests.log`, `/tmp/rw-a04-redb-clippy.log`, `/tmp/rw-a04-redb-linux-tests.log`, `/tmp/rw-a04-redb-release-qualification-final.log`, `/tmp/rw-a04-redb-audit.json`.

## Remaining A04 work

The semantic projector, bounded late-row invalidation queries, rewind/resume jobs, content references, authenticated runtime service, generated protocol, aggregate client cache and native viewport are still required. Existing live replay remains necessary until the separate complete client recovery snapshot can replace it. No claim is made yet about historical UI reachability, production open latency, aggregate client RSS, or complete A04 remediation.
