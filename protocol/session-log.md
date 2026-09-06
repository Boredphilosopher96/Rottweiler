# Durable session event log

A session owns a segmented UTF-8 JSON Lines journal under
`sessions/<id>/journal/`. Sealed segments and `active.jsonl` contain
`SessionEventEnvelope` records; a complete record ends with a newline.
The writer holds `writer.lock` and synchronizes each append batch before
publishing its events. Committed records are immutable.

```json
{"schema_version":1,"sequence":"0","event":{"type":"session_created","meta":{"protocol_version":1,"session_id":"example","sequence_id":"0","emitted_at":"2026-01-01T00:00:00Z"},"driver_client_id":"cli"}}
```

The envelope contract is defined by
[`session-event-envelope.schema.json`](session-event-envelope.schema.json), and
the payload is an [`EngineEvent`](schema/engine-event.schema.json).

## Record validation

- `schema_version` must equal `rw_store::session::SESSION_EVENT_SCHEMA_VERSION`.
  It identifies the envelope independently from the client protocol.
- Every required envelope and payload field must be present. Unknown fields,
  unknown event variants, and mismatched schema identities are rejected.
- `sequence` is a decimal string encoding an unsigned, contiguous, zero-based
  session cursor. It equals `event.meta.sequence_id`; a gap, duplicate,
  mismatch, or reordering is corruption. Connection acknowledgements and
  read responses have no session sequence and are never persisted.
- Crash recovery may discard an unterminated final record in the active segment.
  A newline-terminated malformed record is committed corruption: opening fails
  without rewriting the journal.
- Captured committed-prefix views support bounded cursor pages. Offline
  `rw sessions verify <id>` checks every segment and typed event identity.
- Exports may redact payload content. The durable journal preserves the exact
  provider-neutral and UI-neutral event stream used for replay.

The Rust declarations, generated schemas, record fixtures, recovery tests, and
replay tests must describe this same contract.
