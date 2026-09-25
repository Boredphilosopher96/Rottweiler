import { expect, test } from "bun:test"
import { mkdir, mkdtemp, rm, stat } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { PROTOCOL_VERSION } from "../../src/protocol"
import { TuiEngineRuntime } from "../../src/runtime"
import type { EngineSubscriptionOptions } from "../../src/transport"
import { MemoryFiles, ScriptedClient, TestApp, waitFor } from "./fixtures"

test("cursor handoff cannot recreate a retired runtime directory", async () => {
  const root = await mkdtemp(join(tmpdir(), "rw-cursor-shutdown-"))
  const directory = join(root, "retired-runtime")
  try {
    await mkdir(directory, { mode: 0o700 })
    await rm(directory, { recursive: true })
    const runtime = new TuiEngineRuntime({ socketPath: join(directory, "engine.sock"), bootstrapToken: "secret",
      sessionId: "session-runtime", lastSeenSequence: null, lastSeenFile: join(directory, "last-seen"),
      replayMode: false }, new ScriptedClient())
    runtime.bind(new TestApp())
    await runtime.start()
    await runtime.stop()
    expect(await stat(directory).then(() => true, () => false)).toBeFalse()
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test("stop settles the event stream before flushing its final cursor write", async () => {
  const stream = Promise.withResolvers<void>()
  const write = Promise.withResolvers<void>()
  let aborted = false
  let writing = false
  class LateClient extends ScriptedClient {
    override async subscribe(options: EngineSubscriptionOptions): Promise<void> {
      await super.subscribe(options)
      await new Promise<void>(resolve => {
        const abort = () => { aborted = true; resolve() }
        if (options.signal.aborted) abort()
        else options.signal.addEventListener("abort", abort, { once: true })
      })
      await stream.promise
      await options.onEvent({ type: "mode_changed", mode: "execute", definition_fingerprint: "fixture",
        meta: { protocol_version: PROTOCOL_VERSION, session_id: options.attach.session_id,
          sequence_id: "6", emitted_at: "2026-09-16T00:00:00Z" } })
    }
  }
  class LateFiles extends MemoryFiles {
    override async writePrivateTextAtomic(path: string, content: string, parentPolicy: "create" | "existing"): Promise<void> {
      if (content === "6\n") { writing = true; await write.promise }
      await super.writePrivateTextAtomic(path, content, parentPolicy)
    }
  }
  const client = new LateClient()
  const files = new LateFiles()
  const runtime = new TuiEngineRuntime({ socketPath: "/private/engine.sock", bootstrapToken: "secret",
    sessionId: "session-runtime", lastSeenSequence: null, lastSeenFile: "/private/last-seen", replayMode: false }, client, files)
  runtime.bind(new TestApp())
  const started = runtime.start()
  await waitFor(() => client.commands.some(command => command.type === "list_commands"))
  let stopped = false
  const stopping = runtime.stop().then(() => { stopped = true })
  try {
    await waitFor(() => aborted)
    await Bun.sleep(0)
    expect(stopped).toBeFalse()
    stream.resolve()
    await waitFor(() => writing)
    expect(stopped).toBeFalse()
    write.resolve()
    await stopping
    expect(files.reads.get("/private/last-seen")).toBe("6\n")
  } finally {
    stream.resolve()
    write.resolve()
    await Promise.all([started, stopping])
  }
})
