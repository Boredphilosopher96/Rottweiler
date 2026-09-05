# A04 semantic projection foundation

Status: isolated implementation foundation. Production history readers, client protocol, aggregate cache and native viewport have not migrated.

`rw-types/src/transcript.rs` defines bounded semantic previews and closed canonical-source selectors. Core creates separate conversation, tool invocation, command, shell and child-agent rows. Tool call/result IR remains canonical audit data and does not create duplicate visible rows. Empty provider continuation signatures do not become visible rows or preview payloads. Historical command previews retain the command-time message rather than borrowing a later context/cost snapshot.

`rw-core/src/transcript.rs` interprets contiguous durable events and rejects mismatched sessions/protocols. One event produces at most a row update and an entity binding. A rewind is a distinct operation that cannot advance the semantic watermark on its own. Tool and shell completions retain the original row source while changing its revision and complete-body reference.

`rw-core/src/transcript/projector.rs` owns the derived index and persists its exact interpreted prefix with a constant-sized semantic checkpoint. Normal catch-up reads at most 16 canonical events per transaction and uses a bounded transaction-local row/binding overlay. Input pages retain at most the canonical maximum record size plus its delimiter; source scan ceilings are separate. Preview text is capped before copying; JSON preview serialization aborts at its retained-byte budget. Serialized row size remains independently checked against the index limit.

Rewind deletes a bounded set of later agent-turn rows, then repacks only the affected suffix. Command/shell rows survive. Each phase resumes from the persisted checkpoint. Ordinary page reads refuse incomplete generations. The final transaction publishes the completed ordering and advances through the rewind event together. Total rewind cost is proportional to affected rows; this is not a constant-total-time claim.

## Verification

- All 311 `rw-core` tests passed.
- Strict clippy passed for `rw-core` and `rw-types`, including all targets.
- Seven focused projection tests cover tool start/completion across index reopen, provider-ID reuse with distinct source rows, duplicate tool IR suppression, original shell source retention, bounded UTF-8 and escaped previews, private reasoning signature omission, session/sequence rejection, and transient-event exclusion.
- The mixed lifecycle test uses 139 initial canonical events and appends event 139 during rewind. It reopens the projector after every transaction. All 80 superseded conversation rows disappear, 51 command rows and the original user/tool survive, and the replacement user plus concurrent command remain reachable. Each transaction interprets at most 16 events and writes less than 4 MiB to the index. While repair is hidden, the applied next sequence remains 136. The published result has 55 rows, the complete exact raw prefix, and generation 1. A removed source anchor resolves to its surviving predecessor.
- Updated storage-only 10K/100K backend observations are retained separately in `a04-index-semantic-qualification.jsonl`.

## Remaining adoption gates

A23 is adding host-owned invocation IDs and ensuring failed/recovered calls have a complete start/final lifecycle. This foundation currently follows the existing turn/provider-ID binding and rejects overlapping identities or an unmatched final. That binding will be replaced directly before production adoption; no compatibility adapter is planned. Existing-start recovery must finish the same invocation, while pending IR without a recorded start needs a paired synthetic start.

A02's initial prefix-through implementation was semantically correct but copied/folded the historical metadata catalog. Its replacement is available in `93ed24e` and must be consumed before source-read performance qualification. The index-backend counters here do not measure raw-prefix work.

This foundation does not establish bounded initial client attachment: live/control recovery still uses the existing replay contract. It does not yet provide runtime query admission, a wire page/content API, a full-content cache/reader, aggregate client eviction, or native historical scrolling. Those remain part of A04's production migration.
