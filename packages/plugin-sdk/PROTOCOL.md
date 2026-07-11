# Rottweiler plugin protocol 1

Status: **frozen**. The Rust host launches and passes the tool/hook, event/push, and provider
conformance fixtures; an adversarial plugin-originated undeclared push is killed and reaped. The
schema and fixture beside this file are the language-neutral source of truth; Rust and TypeScript
tests consume them.

Transport is newline-terminated JSON-RPC 2.0 over stdin/stdout. Each JSON value is at most
4 MiB excluding the newline. Empty or unterminated lines, invalid UTF-8/JSON, unknown response
IDs, and malformed envelopes are fatal protocol violations. Requests use bounded integer or
string IDs. Error messages are sanitized strings of at most 16 KiB; arbitrary handler exceptions
must never cross the boundary.

Production loads the expected manifest from trusted configuration before any process is started.
The user approves the exact canonical manifest, origin, argv/cwd/environment/domain configuration,
and content-hashed executable identity. Only then may the host launch the process; the manifest
returned by `initialize` must match the approved fingerprint. An initialization-only probe exists
only in the test harness; it is not a production bootstrap path because a read-broad sandbox cannot
safely execute unapproved code.

Manifest capability arrays contain tools, commands, hooks, provider alias prefixes, event
subscriptions, and plugin-to-host push methods. Tool effects are exactly `reads-fs`, `writes-fs`,
`network`, and `exec`; the host permission engine and process sandbox enforce the immutable
approved declaration. Hooks declare `fail-open` or `fail-closed` and default to a 5-second
deadline. Events are notifications. Pushes are requests and require an explicit declaration.

Canonical methods are `initialize`, `tool/call`, `command/execute`, `hook/invoke`,
`provider/complete`, `provider/event`, `provider/cancel`, `event/publish`,
`session/inject_message`, `session/set_status`, `ui/notify`, `shutdown`, and `exit`. Exact examples
live in `fixtures/wire/protocol-1.json`.

`tool/call` returns `{ "content": string, "data": JSON, "truncated"?: boolean }`.
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

Host requests are bounded by a 5-second default deadline and a 64-request in-flight/writer limit.
Cancellation removes correlation state atomically; late responses to cancelled/timed-out IDs are
ignored up to the bounded abandoned-ID limit. Fatal errors close admission, kill the complete
process tree, and perform a bounded reap. Secret redaction is mandatory before any host value is
serialized to a plugin. Plugin environment inheritance is cleared and restored only from the
small safe allowlist. Approved network plugins receive only canonical public `allowed_domains`;
private, local, link-local, and loopback destinations remain denied by the policy proxy.

Limits: manifest 256 KiB; 256 entries per capability kind; names 128 bytes; version 64 bytes;
descriptions 16 KiB; schemas 64 KiB and depth 32; hook replacements/injected messages 256 KiB.
