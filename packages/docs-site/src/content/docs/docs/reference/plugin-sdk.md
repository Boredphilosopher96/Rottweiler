---
title: TypeScript plugin SDK
description: Scaffold, validate, develop, and package a capability-bound TypeScript plugin.
sidebar:
  order: 3
---

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

## Long-running tools

Tool handlers receive a `ToolHandlerContext`. Use `signal` to stop cooperative
work and `progress` to replace the current observation:

```ts
example_work: async (_params, context) => {
  context.progress({ message: "Reading workspace", amount: { completed: 1, total: 3 } })
  // Perform bounded work and observe context.signal between steps.
  return { content: "Completed", data: null }
}
```

The host supplies immutable total and renewable idle deadlines in
`params.lifetime`. Defaults are five minutes total and ninety seconds idle.
Progress renews only the idle deadline, is coalesced and limited to four deliveries
per second, and is discarded when the operation closes. Messages contain at most
256 Unicode characters without control characters. Counts are unsigned 32-bit
integers with `completed <= total` and `total > 0`.

Progress is transient display state. It does not create durable history, grant
permissions, or prove that cancelled native work stopped. Hook, command and
catalog handlers keep the ordinary five-second deadline.

## Hook contracts

Each declaration requires a hook name, class, and failure policy. Transform
handlers run before policy handlers, followed by observers. A `HookHandler`
receives the input type for its declared event. It can transform only that
phase's mutable fields; tool-call and session identity stay fixed.

Policy hooks can block an operation. Permission policies return `allow`, `ask`,
or `deny`, with the strictest result taking precedence. An `ask` result requires
fresh approval. Observers return `continue` and cannot write to the workspace.

Cancellation aborts `context.signal`. The request remains active until the
handler and its cleanup settle. The host terminates an uncooperative plugin at
its deadline before allowing conflicting work to continue.

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
