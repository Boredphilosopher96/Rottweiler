import { afterEach, describe, expect, test } from "bun:test"
import { rm } from "node:fs/promises"
import {
  EngineRuntimeError,
  TuiEngineRuntime,
  createEngineRuntimeFromEnvironment,
  loadEngineRuntimeConfig
} from "../../src/runtime"
import { MemoryFiles, ScriptedClient } from "./fixtures"

describe("runtime configuration", () => {
  let temporaryDirectory: string | null = null
  afterEach(async () => {
    if (temporaryDirectory !== null) {
      await rm(temporaryDirectory, { recursive: true, force: true })
      temporaryDirectory = null
    }
  })
  test("leaves the visual shell offline when no engine environment is present", async () => {
    const files = new MemoryFiles()
    expect(await loadEngineRuntimeConfig({}, files)).toBeNull()
    expect(await createEngineRuntimeFromEnvironment({ environment: {}, files })).toBeNull()
    expect(files.reads.size).toBe(0)
  })

  test("reads the token once and chooses the newest valid replay cursor", async () => {
    const files = new MemoryFiles()
    files.reads.set("/private/token", "bootstrap-secret\n")
    files.reads.set("/private/cursor", "12\n")

    const config = await loadEngineRuntimeConfig(
      {
        ROTTWEILER_ENGINE_SOCKET: "/private/engine.sock",
        ROTTWEILER_ENGINE_TOKEN_FILE: "/private/token",
        ROTTWEILER_SESSION_ID: "session-runtime",
        ROTTWEILER_LAST_SEEN_SEQUENCE: "9",
        ROTTWEILER_LAST_SEEN_FILE: "/private/cursor",
      },
      files,
    )

    expect(config).toEqual({
      socketPath: "/private/engine.sock",
      bootstrapToken: "bootstrap-secret",
      sessionId: "session-runtime",
      lastSeenSequence: "12",
      lastSeenFile: "/private/cursor",
      replayMode: false,
      forkOperationDirectory: null,
    })
  })

  test("waits for the supervisor token handoff before constructing the runtime", async () => {
    const files = new MemoryFiles()
    const delays: number[] = []
    const runtime = await createEngineRuntimeFromEnvironment({
      environment: {
        ROTTWEILER_ENGINE_SOCKET: "/private/engine.sock",
        ROTTWEILER_ENGINE_TOKEN_FILE: "/private/token",
        ROTTWEILER_SESSION_ID: "session-runtime",
      },
      files,
      client: new ScriptedClient(),
      sleep: async (delay) => {
        delays.push(delay)
        files.reads.set("/private/token", "bootstrap-after-spawn\n")
      },
    })

    expect(runtime).toBeInstanceOf(TuiEngineRuntime)
    expect(delays).toEqual([10])
  })

  test("rejects partial runtime configuration and malformed cursors", async () => {
    const files = new MemoryFiles()
    expect(
      loadEngineRuntimeConfig({ ROTTWEILER_ENGINE_SOCKET: "/private/engine.sock" }, files),
    ).rejects.toEqual(
      new EngineRuntimeError(
        "engine runtime requires ROTTWEILER_ENGINE_SOCKET, ROTTWEILER_ENGINE_TOKEN_FILE, and ROTTWEILER_SESSION_ID",
      ),
    )
    expect(
      loadEngineRuntimeConfig(
        {
          ROTTWEILER_LAST_SEEN_SEQUENCE: "-1",
        },
        files,
      ),
    ).rejects.toEqual(
      new EngineRuntimeError("ROTTWEILER_LAST_SEEN_SEQUENCE must contain a decimal u64 sequence"),
    )
  })
})
