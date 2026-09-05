# `@rottweiler/plugin`

Official zero-runtime-dependency TypeScript SDK for Rottweiler's
newline-delimited JSON-RPC plugin API.

Use `definePlugin` to declare the complete capability manifest and handlers,
then call `runPlugin(plugin)` from the executable entry point. The SDK rejects
handlers that exceed the manifest before the process starts, bounds every input
and output line, keeps protocol output on stdout and writes only explicit debug
labels to stderr.

Provider handlers return an `AsyncIterable<ProviderEvent>`. Events cross the
wire incrementally, and the handler signal aborts when the host drops or
cancels that request. Provider streams are bounded but have no whole-call
five-second deadline.

Provider plugins can declare `capabilities: ["models"]` and implement the
matching `providerModels` handler. Its bounded catalog supplies selectable model
ids, capabilities, limits, and optional integral pricing to the host.

Authenticated providers declare `"credential-references"` on their
provider entry and call `context.providerHttp.request`. The host resolves and
attaches the credential, enforces `allowed_domains`, and streams the response;
the plugin receives the reference and response bytes, never the credential value.

Pushes are JSON-RPC requests and must be listed exactly in `capabilities.push`
before `context.push` will emit them. Every handler receives an `AbortSignal`;
the SDK cancels it on shutdown and request cancellation. Hook, command and catalog
handlers keep the five-second handler deadline. Tools receive host-issued total
and idle deadlines in `params.lifetime` (default five minutes total, ninety seconds
idle). `context.progress({ message, amount?: { completed, total } })` renews only
idle time. Progress is bounded, coalesced and transient; final responses follow
any progress write already in flight. Native timeout or cancellation still requires
the host's process and effect-settlement proof.

`serve()` keeps reading replies and cancellation while handlers run. Handlers
may overlap, so keep mutable plugin state scoped to a request or synchronize it
explicitly. Admission and output queues are bounded; a busy response rejects
new work, while an output overflow or stalled pipe closes the connection.
Always await pushes and respect the handler signal. A handler that ignores
cancellation continues to occupy its admission slot until it actually settles.

`scaffoldTypeScriptPlugin(path, options)` is the deterministic API used by
`rw plugin scaffold --lang ts`. The package also exposes the
`rottweiler-plugin-scaffold` convenience executable.
