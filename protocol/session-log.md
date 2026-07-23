# Durable session event log

Each session is stored as UTF-8 JSON Lines. Every non-empty line is one
`SessionEventEnvelope`; a final newline is required after a complete record.
Writers append records in sequence order and never rewrite committed records.

```json
{"schema_version":1,"sequence":"0","event":{"type":"session_created","meta":{"protocol_version":1,"session_id":"example","sequence_id":"0","emitted_at":"2026-01-01T00:00:00Z"},"driver_client_id":"cli"}}
```

The envelope contract is defined by
[`session-event-envelope.schema.json`](session-event-envelope.schema.json), and
the payload is an [`EngineEvent`](schema/engine-event.schema.json).

## Compatibility

- `schema_version` versions the JSONL envelope independently from the client
  protocol. Readers must reject versions newer than they support.
- `sequence` is a decimal-string encoded unsigned, contiguous, zero-based
  session-local cursor. It equals `event.meta.sequence_id`; a gap, duplicate,
  mismatch, or reordering is corruption. Connection-scoped acknowledgements,
  whose metadata has no session sequence, are never persisted.
- Object fields may be added compatibly. Readers must ignore unknown object
  fields, while writers must continue emitting every required field.
- New event variants require a client-protocol version change. Historical
  variants are never silently reinterpreted.
- A partially written or malformed final line may be discarded during crash
  recovery. A malformed non-final line is corruption and must fail closed.
- Exports may redact payload content, but the durable log itself is the exact
  provider- and UI-neutral event stream used for replay.

The implementation constant is
`rw_store::session::SESSION_EVENT_SCHEMA_VERSION`. Changes to the envelope must
update that constant, this document, the checked-in schema, recovery tests, and
replay compatibility tests in one change.
