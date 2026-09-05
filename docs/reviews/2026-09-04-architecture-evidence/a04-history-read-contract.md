# A04 history availability and semantic summary contract

The source-owned protocol distinguishes `SessionHistoryReady` from actual raw
replay completion. It is a connection event; its advertised tail cannot advance
the client durable cursor or manufacture a replay receipt. Runtime exposes a
read-only `HistoryReader` capability for semantic pages and content, with request,
connection-generation and session checks before returning data to its caller.
Read replies must match the command's source-owned execution class.

`ClientCommand::execution` and the generated TypeScript execution map now share
one Rust variant declaration. TypeScript does not maintain a second method list.
The generated transcript projection-version constant has the same Rust owner.

Projection version 2 adds `TurnSummary`, sourced only from `TurnFinished` and
carrying exact turn identity, status, usage and cost. Provider call receipts do
not create display rows. Summary rows obey ordinary byte/count paging and
rewind ownership. The regression includes maximum u64 usage and monetary values,
a separate provider receipt, a one-row page, and removal of a later summary by
rewind.

Validation in `/tmp/rw-client-bounds`, using its private Cargo target and pinned
Bun 1.3.14:

- Core transcript suite: 15 passed, one ignored diagnostic performance probe.
- Transport/history-availability regressions: 8 passed, 20 assertions.
- Strict all-target Clippy for core, types and xtask passed.
- `cargo xtask codegen --check` passed; TUI typecheck passed.

This checkpoint establishes the wire and semantic contracts. Native client
window/cache adoption and historical CLI production migration are a separate
ongoing unit; these checks do not claim that migration is complete.
