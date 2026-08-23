The Client API keeps session logic in the Rust engine. Clients render events
and send commands; they do not own routing, tools, persistence, permissions,
compaction, or replay.

## Behavior

- Clients establish a connection and receive an initial session snapshot.
- Every mutating command carries the identity required by the engine boundary.
- Engine events are ordered and use generated schemas shared with TypeScript.
- Disconnecting a client does not make that client the owner of the session.

## Canonical artifacts

- [Client command schema](/Rottweiler/generated/client/client-command.schema.json)
- [Engine event schema](/Rottweiler/generated/client/engine-event.schema.json)
- [Block schema](/Rottweiler/generated/client/block.schema.json)
- [Tool output schema](/Rottweiler/generated/client/tool-output.schema.json)
- [Generated TypeScript](/Rottweiler/generated/client/types.ts)

The same code generator owns the Rust and TypeScript projections and the
fixtures used for cross-language contract tests.
