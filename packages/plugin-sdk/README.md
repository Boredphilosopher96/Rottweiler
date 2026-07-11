# `@rottweiler/plugin`

Official zero-runtime-dependency TypeScript SDK for Rottweiler's frozen
newline-delimited JSON-RPC 2.0 plugin protocol 1.

Use `definePlugin` to declare the complete capability manifest and handlers,
then call `runPlugin(plugin)` from the executable entry point. The SDK rejects
handlers that exceed the manifest before the process starts, bounds every input
and output line, keeps protocol output on stdout and writes only explicit debug
labels to stderr.

Provider handlers return an `AsyncIterable<ProviderEvent>`. Events cross the
wire incrementally, and the handler signal aborts when the host drops or
cancels that request. Provider streams are bounded but have no whole-call
five-second deadline.

Pushes are JSON-RPC requests and must be listed exactly in `capabilities.push`
before `context.push` will emit them. Every handler receives an `AbortSignal`;
the SDK cancels it on shutdown, request cancellation, and—except for provider
streams—after the bounded handler timeout (5 seconds by default).

`scaffoldTypeScriptPlugin(path, options)` is the deterministic API used by
`rw plugin scaffold --lang ts`. The package also exposes the
`rottweiler-plugin-scaffold` convenience executable.
