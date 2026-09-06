import { definePlugin, runPlugin } from "../../src/index.ts"

const probe = "native_probe"
const read = "scoped_read"
const write = "scoped_write"
const manifest = {
  name: "code-only-effects", version: "1.0.0", protocol: 3,
  capabilities: {
    tools: [
      { name: probe, description: "Probe denied ambient effects", schema: { type: "object" }, caps: ["reads-fs", "writes-fs", "network", "exec"] },
      { name: read, description: "Read through owned host scope", schema: { type: "object" }, caps: ["reads-fs"] },
      { name: write, description: "Write through owned host scope", schema: { type: "object" }, caps: ["reads-fs", "writes-fs"] },
    ],
    push: ["effect/tool_call"],
  },
} as const
async function denied(operation: () => Promise<unknown>): Promise<boolean> {
  try { await operation(); return false } catch { return true }
}
const plugin = definePlugin({ manifest, handlers: { tools: {
  [probe]: async ({ input }) => {
  const value = input as { secret: string; output: string; url: string }
  const results = {
    read: await denied(() => Bun.file(value.secret).text()),
    write: await denied(() => Bun.write(value.output, "unowned mutation")),
    process: await denied(async () => { const child = Bun.spawn(["/usr/bin/true"]); await child.exited }),
    network: await denied(() => fetch(value.url, { signal: AbortSignal.timeout(1000) })),
  }
  return { content: JSON.stringify(results), data: results, truncated: false }
},
  [read]: async ({ input }, { effects }) => effects.callTool("read", input),
  [write]: async ({ input }, { effects }) => effects.callTool("write", input),
} } })
if (import.meta.main) await runPlugin(plugin)
