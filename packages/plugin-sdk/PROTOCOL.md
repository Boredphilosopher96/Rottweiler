# Rottweiler plugin API

The dependency-leaf `rw-plugin-protocol` crate owns the contract and generates
the TypeScript, schema, and fixture projections beside this file.

## Initialization

The host sends `initialize` with `protocol`, `min_protocol`, and an optional bounded string
`capabilities` list. A plugin's approved manifest must select the version offered
by the host. Declared model discovery requires `provider-models`, and declared
credential references require `provider-http`. Unknown host capability strings
do not grant authority.

Transport is newline-terminated JSON-RPC 2.0 over stdin/stdout. Empty or unterminated lines,
invalid UTF-8/JSON, unknown response IDs, and malformed envelopes are fatal protocol violations.
Requests use bounded integer or string IDs. Error messages are sanitized; arbitrary handler
exceptions must never cross the boundary. The generated `PROTOCOL_LIMITS` object and current
schema project the authoritative bounds.

Production loads the expected manifest from trusted configuration before any process is started.
The user approves the exact canonical manifest, origin, argv/cwd/environment/domain configuration,
and content-hashed executable identity. Only then may the host launch the process; the manifest
returned by `initialize` must match the approved fingerprint. An initialization-only probe exists
only in the test harness; it is not a production bootstrap path because a read-broad sandbox cannot
safely execute unapproved code.

Manifest capability arrays contain tools, commands, hooks, provider alias prefixes, event
subscriptions, and plugin-to-host push methods. Tool effects are exactly `reads-fs`, `writes-fs`,
`network`, and `exec`; the host permission engine and process sandbox enforce the immutable
approved declaration. Hooks declare `fail-open` or `fail-closed` and use the generated default
handler timeout. Events are notifications. Pushes are requests and require an explicit declaration.

The generated `RPC_METHODS` object is the canonical method catalog. The
generated wire fixture contains exact examples.

`tool/call` returns `{ "content": string, "data": JSON, "truncated"?: boolean }`.
Provider declarations opt into catalog RPC with
`"capabilities": ["models"]`; this declaration is part of the approved manifest fingerprint.
Other bounded canonical capability strings are retained in that fingerprint but confer no host
authority unless the host recognizes and negotiates them.
`provider/models` receives `{ "alias_prefix": string }` and returns `{ "models": [...] }`.
Each model contains a bounded provider-local `id`, optional `display_name`, required
`capabilities` (`tool_calling`, `vision`, `thinking`, and `cache_breakpoints`, whose value is
`none`, `explicit`, or `automatic`), optional `max_context_tokens` / `max_output_tokens`, and
optional integral micro-US-dollar per-million-token `pricing`. Catalog data flows only from the
plugin to the host. It never exposes host credentials or bypasses the approved alias prefix.

Provider declarations may also list bounded `credential-references`. The list is part
of the approved manifest fingerprint, and the host refuses an alias/reference pair absent from
that exact provider declaration. `context.providerHttp.request` sends only the reference plus a
credential-free HTTP request. The host resolves and registers the secret, attaches it to the
declared header, and owns the guarded socket. Responses arrive as correlated `head`, bounded
base64 `body`, and `finished` events; dropping the body or aborting the provider sends
`provider/http_cancel`. Neither the secret nor an authenticated request representation is ever
serialized to the plugin. The same `allowed_domains`, public-address rail, process-wide network
deny guard, response bounds, and backpressure used by guarded provider HTTP apply here.

`provider/complete` receives Rottweiler's provider-neutral request (`model`, `turns`, `tools`,
tagged `tool_choice`, `max_output_tokens`, nullable `temperature`, `thinking`, optional
`cache_hint`). Its original JSON-RPC ID is the stream correlation ID. The plugin emits each tagged
normalized event immediately as a `provider/event` notification with
`{ "request_id": <original-id>, "event": ... }`, emits exactly one terminal `finished` event, then
answers the original request with `result: null`. The host bounds each request's event queue and
kills a producer that overruns backpressure, emits malformed/out-of-order events, or crosses
correlation. Dropping the consumer sends `provider/cancel` with the same request ID; the SDK aborts
the handler's signal and closes the async iterator. Provider streams have bounded admission and
write deadlines but deliberately have no five-second whole-call deadline. Other handlers retain
the default five-second deadline.

The SDK input pump routes correlated HTTP replies and cancellation independently
of application handlers. Ordinary handlers can run concurrently, including a
catalog handler awaiting host-mediated HTTP. The SDK admits at most 64 handler
invocations; timed-out invocations keep their slot until the underlying handler
settles. New requests beyond that limit receive `-32005` (busy).

The SDK's output FIFO includes the active write in its 16 MiB and 256-frame
limits. Overflow, write failure, or a write deadline closes the connection and
settles pending writers. The server uses its configured handler timeout as the
write deadline. Host HTTP requests have separate bounded admission and body
queues; an overflowing body is cancelled without blocking the input pump.
These are local admission policies, owned by the SDK server and transport;
generated protocol limits continue to own individual wire-value bounds.

Host requests use the generated default deadline and a separately enforced bounded in-flight/writer limit.
Cancellation removes correlation state atomically; late responses to cancelled/timed-out IDs are
ignored up to the bounded abandoned-ID limit. Fatal errors close admission, kill the complete
process tree, and perform a bounded reap. Secret redaction is mandatory before any host value is
serialized to a plugin. Plugin environment inheritance is cleared and restored only from the
small safe allowlist. Approved network plugins receive only canonical public `allowed_domains`;
private, local, link-local, and loopback destinations remain denied by the policy proxy.

All frame, manifest, capability, name, schema, payload, catalog, token, and pricing bounds come
from `rw-plugin-protocol` and its generated `PROTOCOL_LIMITS` and schema projections. The Rust
boundary clamps bounded catalog values, and the SDK rejects values outside the same generated
ranges. A malformed catalog fails discovery for that provider only and does not terminate startup
or the session.

## Correlated host commands

`push.injectMessage`, `push.setStatus`, and `push.notify` await the correlated
host response. Injection returns `{ disposition: "started" | "queued" | "command" }`;
status and notification commands resolve only after the host applies them.
Capability, session, and parameter failures reject the promise. Pending host
commands are limited to 64 per plugin process. Active request IDs must be unique;
a duplicate active ID is a terminal protocol violation.

A local deadline, cancellation, or disconnect rejects with **outcome unknown**:
it does not undo an admitted host command, and retrying it may duplicate a
mutation. The host retains ownership until the actor replies, including during
process teardown. Commands require request IDs; these methods are not
fire-and-forget notifications.
