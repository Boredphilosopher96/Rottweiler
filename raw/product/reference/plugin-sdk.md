The TypeScript SDK supplies the typed manifest, handlers, server, and project
scaffold used by Rottweiler plugins.

## Public exports

- `definePlugin(definition)` validates and locks a typed plugin definition.
- `parsePluginManifest(value)` validates an external manifest at the boundary.
- `runPlugin(plugin)` serves the plugin over the bounded stdio API.
- `PluginServer` exposes the lower-level server lifecycle.
- `SafeRpcError` returns an intentional bounded RPC error.
- Protocol request, response, capability, manifest, context, and contribution
  types are exported from the package root.
- The `./scaffold` export owns deterministic plugin project generation.

## Minimal plugin

```ts
import { definePlugin, parsePluginManifest, runPlugin } from "@rottweiler/plugin"
import manifestDocument from "../manifest.json"

const plugin = definePlugin({
  manifest: parsePluginManifest(manifestDocument),
  handlers: {
    tools: {
      example_echo: ({ input }) => ({
        content: String(input.message ?? "hello"),
        data: input,
      }),
    },
  },
})

await runPlugin(plugin)
```

The manifest is the capability boundary. Declaring a capability does not by
itself grant approval; the host binds approval to the manifest fingerprint.

## Scaffold a project

```sh
rw plugin scaffold --lang ts --name example.tools ./example-plugin
cd example-plugin
rw plugin check . --allow-exec
bun run build
```

`plugin check` verifies that `manifest.json` and `package.json` name the same
plugin, then runs the package's required `typecheck` and `test` scripts. It does
not attach the plugin to a live session.

The source target is exactly `source`. An executable target is exactly an
argument vector plus an optional working directory.

## Develop against a live session

```sh
rw plugin dev ./example-plugin --session current --allow-dev-exec
```

The development command performs bounded rebuild, validation, activation, and
shutdown. Manifest and source changes rerun the declared build rather than
patching a running plugin.

See the [Plugin API](./plugin-api.md) for the language-neutral contract.
