# A04 semantic projection foundation

Status: isolated implementation foundation. Production history readers, client protocol, aggregate cache and native viewport have not migrated.

`rw-types/src/transcript.rs` defines bounded semantic previews and closed canonical-source selectors. Core creates separate conversation, tool invocation, command, shell and child-agent rows. Tool call/result IR remains canonical audit data and does not create duplicate visible rows. Empty provider continuation signatures do not become visible rows or preview payloads. Historical command previews retain the command-time message rather than borrowing a later context/cost snapshot.

`rw-core/src/transcript.rs` interprets contiguous durable events and rejects mismatched sessions/protocols. One event produces at most a row update and an entity binding. A rewind is a distinct operation that cannot advance the semantic watermark on its own. Tool and shell completions retain the original row source while changing its revision and complete-body reference.

`rw-core/src/transcript/projector.rs` owns the derived index and persists its exact interpreted prefix with a constant-sized semantic checkpoint. Normal catch-up reads at most 64 canonical events per transaction, independently limited to 128 mutations and 1 MiB of charged output, and uses a bounded transaction-local row/binding overlay. Input pages retain at most the canonical maximum record size plus its delimiter; source scan ceilings are separate. Preview text is capped before copying; JSON preview serialization aborts at its retained-byte budget. Serialized row size remains independently checked against the index limit.

Rewind deletes a bounded set of later agent-turn rows, then repacks only the affected suffix. Command/shell rows survive. Each phase resumes from the persisted checkpoint. Ordinary page reads refuse incomplete generations. The final transaction publishes the completed ordering and advances through the rewind event together. Total rewind cost is proportional to affected rows; this is not a constant-total-time claim.

`rw-core/src/transcript/content.rs` prepares one complete display document from a closed canonical selector. Plain text moves from the decoded event into the document; structured content serializes once through a capped writer. Subsequent reads return borrowed UTF-8 slices, with at most three byte backsteps to a character boundary, rather than decoding or copying the complete source repeatedly. Retained capacity is exposed for runtime cache accounting. Conversation documents exclude private reasoning signatures and duplicate tool IR. Runtime session/prefix authentication and aggregate document admission remain adoption work.

Finished tool and child states require their final body in the enum variant. Partial shell updates preserve previous command, output and status source references.

## Verification

- All 318 `rw-core` and 163 `rw-store` tests passed (qualification tests remain ignored by default).
- Strict clippy passed for `rw-core`, `rw-store`, and `rw-types`, including all targets.
- Thirteen focused projection/content tests cover tool start/completion across index reopen, provider-ID reuse with distinct source rows, duplicate tool IR suppression, original shell source retention, bounded UTF-8 and escaped previews, private reasoning signature omission, session/sequence rejection, and transient-event exclusion.
- The mixed lifecycle test uses 139 initial canonical events and appends event 139 during rewind. It reopens the projector after every transaction. All 80 superseded conversation rows disappear, 51 command rows and the original user/tool survive, and the replacement user plus concurrent command remain reachable. Each normal transaction interprets at most 64 events; rewind transactions process at most 16 affected rows and writes less than 4 MiB to the index. While repair is hidden, the applied next sequence remains 136. The published result has 55 rows, the complete exact raw prefix, and generation 1. A removed source anchor resolves to its surviving predecessor.
- An escaped-payload regression stops a raw page at the independent output-byte limit and resumes at the exact processed sequence without skipping any of 160 conversation events. A separate tool lifecycle test crosses the 64-event raw page boundary and reopens the index before applying the final.
- A 10K-event qualification uses 7,786,670 canonical bytes and checks first/middle/last page access. Increasing the input batch from 16 to 64 and removing redundant prefix verification reduced the local debug build sample from 48.551 s / 625 batches / 39,040,512 index bytes written to 10.131 s / 157 batches / 16,493,824 bytes written. The same 64-event implementation took 1.742 s in release mode. All index transactions stayed below 4 MiB of writes. These are single local samples, not p95, hosted, or end-to-end UI evidence; raw records are in `a04-semantic-build-qualification.jsonl`.
- Content tests prove that plain text keeps its original allocation across all chunks; malformed UTF-8 offsets and non-progressing limits fail explicitly. Escaped structured JSON hits the body allocation ceiling and reassembles correctly from small chunks when admitted. Whole-conversation and individual reasoning reads omit private continuation signatures, selector mismatches fail, and child completion retains one row with a bounded preview and authoritative complete-result source.
- Updated storage-only 10K/100K backend observations are retained separately in `a04-index-semantic-qualification.jsonl`.

## Remaining adoption gates

A23 is adding host-owned invocation IDs and ensuring failed/recovered calls have a complete start/final lifecycle. This foundation currently follows the existing turn/provider-ID binding and rejects overlapping identities or an unmatched final. That binding will be replaced directly before production adoption; no compatibility adapter is planned. Existing-start recovery must finish the same invocation, while pending IR without a recorded start needs a paired synthetic start.

A02's shared catalog replacement (`93ed24e`) is consumed by this foundation. Raw pages and exact prefix validation can still reread a bounded boundary segment; the index-backend counters do not measure that source work.

This foundation does not establish bounded initial client attachment: live/control recovery still uses the existing replay contract. It does not yet provide runtime query admission, a wire page/content API, a full-content cache/reader, aggregate client eviction, or native historical scrolling. Those remain part of A04's production migration.
