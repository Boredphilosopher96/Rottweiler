# Generated protocol contract

Rust types in `crates/rw-types` are the only source of truth. Do not edit the
schemas or `types.ts` by hand.

```console
cargo xtask codegen
cargo xtask codegen --check
```

The first command refreshes committed artifacts. The second exits non-zero when
the generated output differs, and is the CI drift gate. Protocol objects reject
unknown fields, unsupported variants, and missing required fields.

Durable sessions use the separately versioned public JSONL envelope documented
in [`session-log.md`](session-log.md). Its machine-readable envelope is
[`session-event-envelope.schema.json`](session-event-envelope.schema.json); the
`event` field uses the generated
[`EngineEvent`](schema/engine-event.schema.json) contract.
