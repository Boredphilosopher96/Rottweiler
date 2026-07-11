import { afterEach, describe, expect, test } from "bun:test"
import { readFile } from "node:fs/promises"
import { fileURLToPath } from "node:url"
import {
  createTestRenderer,
  MockTreeSitterClient,
  type TestRenderer,
} from "@opentui/core/testing"

import { createRottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type EngineEvent } from "../../src/protocol"
import { TuiEngineRuntime } from "../../src/runtime"
import { EngineHttpSseClient, isWireEngineEvent } from "../../src/transport"
import {
  AuthenticatedMockEngine,
  encodeSseJson,
  splitBytes,
} from "../support/mock-engine"

const SESSION_ID = "session-m9-replay-golden"
const FIXTURE = fileURLToPath(new URL("../fixtures/m9-replay-events.jsonl", import.meta.url))

describe("M9 persisted replay golden", () => {
  let renderer: TestRenderer | undefined
  let treeSitter: MockTreeSitterClient | undefined
  let engine: AuthenticatedMockEngine | undefined

  afterEach(async () => {
    renderer?.destroy()
    renderer = undefined
    await treeSitter?.destroy()
    treeSitter = undefined
    await engine?.stop()
    engine = undefined
  })

  test("authenticated observer replay renders the persisted event log", async () => {
    const events = await readPersistedEvents()
    const completed = {
      type: "session_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "m9-replay-client",
        request_id: "m9-replay-completed",
        emitted_at: "2026-01-01T00:00:10Z",
      },
      session_id: SESSION_ID,
      through_sequence: "9",
    } satisfies EngineEvent
    const bytes = Buffer.concat([...events, completed].map((event) => encodeSseJson(event)))
    engine = new AuthenticatedMockEngine([
      { chunks: splitBytes(bytes, [1, 7, 31, 2, 127, 5, 509]), holdOpen: true },
    ])
    await engine.start()

    const setup = await createTestRenderer({ width: 112, height: 32, useThread: false })
    renderer = setup.renderer
    treeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
    treeSitter.setMockResult({ highlights: [] })
    const app = createRottweilerApp(renderer, {
      sessionId: SESSION_ID,
      replaySessionId: SESSION_ID,
      treeSitterClient: treeSitter,
    })
    renderer.root.add(app)

    const client = new EngineHttpSseClient({
      socketPath: engine.socketPath,
      bootstrapToken: engine.bootstrapToken,
    })
    const runtime = new TuiEngineRuntime(
      {
        socketPath: engine.socketPath,
        bootstrapToken: engine.bootstrapToken,
        sessionId: SESSION_ID,
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: true,
      },
      client,
      undefined,
      () => "m9-runtime-request",
    )
    runtime.bind(app)
    const running = runtime.start()
    try {
      await waitFor(() => app.state.replay.completedThrough === "9")
      await setup.waitFor(() => treeSitter?.isHighlighting() === false)
      await setup.flush()

      expect(engine.commands).toHaveLength(1)
      expect(engine.commands[0]).toMatchObject({
        type: "attach_session",
        session_id: SESSION_ID,
        role: "observer",
        last_seen_sequence: null,
      })
      expect(app.state.lastSequence).toBe("9")
      expect(app.state.transcript).toHaveLength(2)

      const frame = setup
        .captureCharFrame()
        .split("\n")
        .map((line) => line.trimEnd())
        .join("\n")
      const styled = setup.captureSpans().lines.map((line) =>
        line.spans
          .filter((span) => span.text.trim().length > 0)
          .map((span) => [span.text, span.fg.toInts(), span.bg.toInts(), span.attributes]),
      )
      expect(
        JSON.stringify({
          frame,
          styledDigest: stableDigest(JSON.stringify(styled)),
          styledSpanCount: styled.reduce((total, line) => total + line.length, 0),
        }),
      ).toMatchSnapshot()
    } finally {
      await runtime.stop()
      await running
    }
  })
})

async function readPersistedEvents(): Promise<EngineEvent[]> {
  const lines = (await readFile(FIXTURE, "utf8")).trim().split("\n")
  return lines.map((line, index) => {
    const value: unknown = JSON.parse(line)
    if (!isWireEngineEvent(value) || value.type === "session_replay_completed") {
      throw new Error(`invalid persisted replay event at line ${index + 1}`)
    }
    return value as EngineEvent
  })
}

async function waitFor(predicate: () => boolean, timeoutMs = 2_000): Promise<void> {
  const deadline = performance.now() + timeoutMs
  while (!predicate()) {
    if (performance.now() >= deadline) throw new Error("timed out waiting for persisted replay")
    await Bun.sleep(5)
  }
}

function stableDigest(value: string): string {
  let hash = 2_166_136_261
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index)
    hash = Math.imul(hash, 16_777_619)
  }
  return (hash >>> 0).toString(16).padStart(8, "0")
}
