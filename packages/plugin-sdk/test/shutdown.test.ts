import { expect, test } from "bun:test"
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { setImmediate as nextTurn } from "node:timers/promises"
import { PluginServer, PROTOCOL_LIMITS } from "../src/index"

test("shutdown retains an ignored-abort handler after transport deadline until its effect settles", async () => {
  const root = await mkdtemp(join(tmpdir(), "rw-sdk-shutdown-"))
  const marker = join(root, "settled")
  const release = Promise.withResolvers<void>()
  const timedOut = Promise.withResolvers<void>()
  let completed = false
  let entered = false
  const server = new PluginServer({
    manifest: { name: "shutdown-owner", version: "1", protocol: 3, capabilities: {} },
    handlers: { shutdown: async (signal) => {
      expect(signal.aborted).toBe(true)
      entered = true
      await release.promise
      await writeFile(marker, "effect settled after cancellation")
    } },
  }, {
    input: (async function* () {})(),
    output: { write() {} },
    error: { write(message) { if (message.includes("shutdown timed out")) timedOut.resolve() } },
  }, PROTOCOL_LIMITS.maxLineBytes, 5)
  const shutdown = server.shutdown()
  void shutdown.then(() => { completed = true })
  try {
    expect(server.shutdown()).toBe(shutdown)
    await timedOut.promise
    await nextTurn()
    expect(entered).toBe(true)
    expect(await Bun.file(marker).exists()).toBe(false)
    expect(completed).toBe(false)
    release.resolve()
    await shutdown
    expect(await readFile(marker, "utf8")).toBe("effect settled after cancellation")
    expect(completed).toBe(true)
  } finally {
    release.resolve()
    await shutdown
    await rm(root, { recursive: true, force: true })
  }
})
