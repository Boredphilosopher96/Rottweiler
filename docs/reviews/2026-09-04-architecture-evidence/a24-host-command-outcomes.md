# Correlated host command outcomes: first lifecycle checkpoint

This checkpoint addresses A24 and the host-reader command head-of-line portion of
A06. ADR-031 records the intended larger lifecycle contract. Provider credits,
finite long-tool operations, provider settlement forwarding, and protocol-3
migration are subsequent parts of that unit; this checkpoint still speaks
protocol 2 and does not claim those parts complete.

The host admits at most 64 commands plus 64 host HTTP operations. Admitted
commands run in owned tasks, retaining their settlement permit through the
actual actor reply. The reader continues correlating replies and control frames.
The former five-second wrapper around the actor future is removed: its timeout
could drop the reply future after an actor command was already queued.

SDK commands await their correlated response. Injection returns the typed
started/queued/command disposition; status and notification require null success.
Host errors and malformed outcomes reject the promise. Pending SDK commands are
bounded to 64, and disconnect and deadline failures mean outcome unknown, not
rollback. Active duplicate host-command IDs fail the process protocol. Commands
must use request envelopes, not notifications.

A native handler timeout/cancel error starts the host's A21 process settlement
before the ordinary request returns. The existing SDK may report cancellation
while uncooperative application code remains alive; this response alone cannot
prove that local effects stopped.

Validation on macOS with pinned Bun 1.3.14:

- `cargo test -p rw-ext`: 115 tests passed. The delayed actor regression now
  verifies an unrelated ping is answered while the actor waits, then holds the
  actor reply beyond five seconds while cancellation remains pending.
- SDK `bun test`: 64 passed, including actual input-pump correlated injection,
  host rejection, malformed response, and disconnect cases.
- SDK `bun run typecheck`, protocol codegen `--check`, `cargo fmt --all --check`,
  and scoped all-target clippy for rw-ext/rw-runtime/rw-plugin-protocol passed.

This is local source validation. No native Linux run or hosted release/soak
qualification was performed for this checkpoint.

Adversarial follow-up: a host command lease now keeps its permit charged if the
owner panics or disappears after admission, because task destruction does not
prove the actor stopped. The new panic regression enqueues an independent actor
command, panics, releases that command later, and verifies settlement stays
pending. Active IDs remain charged through response enqueue or teardown. The
panic and delayed-command focused tests and rw-ext all-target clippy passed.
