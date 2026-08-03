# Rottweiler plugin protocol 2

Status: **stable**. Protocol 1 remains frozen and supported unchanged. The protocol-2 schema and
fixture beside this file are the current language-neutral source of truth; Rust and TypeScript
tests consume both protocol generations.

## Version negotiation and compatibility

The host sends `initialize` with `protocol` (the selected/highest mutually supported version),
`min_protocol` (its lowest supported version), and an optional bounded string `capabilities` list.
The selected version in the plugin's approved manifest must fall in that inclusive range.
Protocol 2 requires `provider-models` for declared model discovery and `provider-http` for any
declared credential reference. Unknown host capability strings are ignored, so later additive
facilities remain negotiable without protocol 3.

The engine accepts protocol 2 and N-1 (protocol 1). Protocol-1 manifests and wire behavior remain
unchanged and receive the conservative provider defaults documented below. A protocol version is
deprecated for at least one stable release before removal; removal may occur only when it is older
than N-1 and must be announced in release notes. Additive optional methods and capability strings
may be introduced within protocol 2. Breaking wire-shape, framing, or existing-method semantic
changes require protocol 3.

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
`provider/complete`, `provider/models`, `provider/event`, `provider/cancel`, `event/publish`,
`provider/http`, `provider/http_event`, `provider/http_cancel`, `session/inject_message`,
`session/set_status`, `ui/notify`, `shutdown`, and `exit`. Exact examples
live in `fixtures/wire/protocol-1.json` and `fixtures/wire/protocol-2.json`.

`tool/call` returns `{ "content": string, "data": JSON, "truncated"?: boolean }`.
Protocol-2 provider declarations opt into catalog RPC with
`"capabilities": ["models"]`; this declaration is part of the approved manifest fingerprint.
Other bounded canonical capability strings are retained in that fingerprint but confer no host
authority unless the host recognizes and negotiates them.
`provider/models` receives `{ "alias_prefix": string }` and returns `{ "models": [...] }`.
Each model contains a bounded provider-local `id`, optional `display_name`, required
`capabilities` (`tool_calling`, `vision`, `thinking`, and `cache_breakpoints`, whose value is
`none`, `explicit`, or `automatic`), optional `max_context_tokens` / `max_output_tokens`, and
optional integral micro-US-dollar per-million-token `pricing`. Catalog data flows only from the
plugin to the host. It never exposes host credentials or bypasses the approved alias prefix.

Protocol-1 providers do not receive `provider/models` and retain `tool_calling: true`, all other
feature flags false, no cache breakpoints, no token limits, and unpriced API accounting.
Protocol-2 provider declarations may also list bounded `credential-references`. The list is part
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

Host requests are bounded by a 5-second default deadline and a 64-request in-flight/writer limit.
Cancellation removes correlation state atomically; late responses to cancelled/timed-out IDs are
ignored up to the bounded abandoned-ID limit. Fatal errors close admission, kill the complete
process tree, and perform a bounded reap. Secret redaction is mandatory before any host value is
serialized to a plugin. Plugin environment inheritance is cleared and restored only from the
small safe allowlist. Approved network plugins receive only canonical public `allowed_domains`;
private, local, link-local, and loopback destinations remain denied by the policy proxy.

Limits: manifest 256 KiB; 256 entries per capability kind; names 128 bytes; version 64 bytes;
descriptions 16 KiB; schemas 64 KiB and depth 32; hook replacements/injected messages 256 KiB.
Catalogs contain at most 256 models. Model ids/display names are at most 128 bytes. Declared token
limits are clamped to 1..16,777,216 and prices to 0..1,000,000,000,000 micro-USD per million
tokens by the Rust boundary; the SDK rejects values outside those ranges. A malformed catalog
fails discovery for that provider only and does not terminate startup or the session.
