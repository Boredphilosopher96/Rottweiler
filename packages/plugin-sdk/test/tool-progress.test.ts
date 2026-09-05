import { expect, test } from "bun:test"
import { ToolProgressReporter } from "../src/tool-progress"
import { PluginServer, definePlugin, PROTOCOL_LIMITS, type JsonValue } from "../src/index"

const lifetime = { total_ms: 800, idle_ms: 450 }
const initialize = { host: "rottweiler", protocol: 3, min_protocol: 3, max_frame_bytes: PROTOCOL_LIMITS.maxLineBytes }

test("one active write and one replaceable progress survive a producer flood", async () => {
  let release!: () => void
  const blocked = new Promise<void>(resolve => { release = resolve })
  const writes: string[] = []
  let delivered = 0
  const reporter = new ToolProgressReporter(async (_sequence, progress) => {
    writes.push(progress.message)
    await blocked
  }, () => { delivered += 1 }, () => { throw new Error("unexpected delivery failure") })
  reporter.report({ message: "first" })
  for (let index = 0; index < 100_000; index += 1) reporter.report({ message: `latest ${index}` })
  expect(writes).toEqual(["first"])
  let finished = false
  const completion = reporter.finish().then(() => { finished = true })
  await Bun.sleep(1)
  expect(finished).toBe(false)
  release()
  await completion
  expect(writes).toEqual(["first"])
  expect(delivered).toBe(0)
  expect(() => reporter.report({ message: "late" })).toThrow("closed")
})

test("progress snapshots validate UTF-8, controls, and relational count bounds", async () => {
  const reporter = new ToolProgressReporter(async () => {}, () => {}, () => {})
  for (const message of ["", "bad\x1b", "é".repeat(257)]) {
    expect(() => reporter.report({ message })).toThrow("invalid tool progress")
  }
  expect(() => reporter.report({ message: "work", amount: { completed: 2, total: 1 } })).toThrow()
  await reporter.finish()
})

test("typed progress renews idle only and leaves control responsive until total expiry", async () => {
  const frames: Array<Record<string, JsonValue>> = []
  let observedAbort = false
  let reporter: ReturnType<typeof setInterval> | undefined
  const server = new PluginServer(definePlugin({
    manifest: { name: "progress", version: "1", protocol: 3, capabilities: {
      tools: [{ name: "work", description: "work", schema: {}, caps: [] }],
      hooks: [{ name: "pre_tool", failure_policy: "fail-closed" }],
    } },
    handlers: {
      tools: { work: (_params, context) => new Promise<never>(() => {
        context.progress({ message: "started" })
        reporter = setInterval(() => context.progress({ message: "working" }), 100)
        context.signal.addEventListener("abort", () => {
          observedAbort = true
          clearInterval(reporter)
        }, { once: true })
      }) },
      hooks: { pre_tool: () => ({ decision: "allow" }) },
    },
  }), { input: (async function* () {})(), output: { write(bytes) {
    frames.push(JSON.parse(new TextDecoder().decode(bytes)))
  } } }, 4096, 50)
  const send = (id: number, method: string, params: JsonValue) => server.handleLine(JSON.stringify({ jsonrpc: "2.0", id, method, params }))
  await send(1, "initialize", initialize)
  const started = performance.now()
  const tool = send(2, "tool/call", { name: "work", input: {}, lifetime })
  await Bun.sleep(100)
  await send(3, "hook/invoke", { hook: "pre_tool", payload: {} })
  expect(frames.find(frame => frame.id === 3)).toMatchObject({ result: { decision: "allow" } })
  expect(frames.some(frame => frame.id === 2)).toBe(false)
  await tool
  const elapsed = performance.now() - started
  expect(elapsed).toBeGreaterThanOrEqual(750)
  expect(elapsed).toBeLessThan(1500)
  expect(observedAbort).toBe(true)
  expect(frames.at(-1)).toMatchObject({ id: 2, error: { code: -32004 } })
  expect(frames.filter(frame => frame.method === "tool/progress").length).toBeGreaterThan(1)
  const count = frames.length
  await Bun.sleep(275)
  expect(frames.length).toBe(count)
  await server.shutdown()
})
