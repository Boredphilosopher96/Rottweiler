import { expect } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp,
  type PresentationFrameScheduler
} from "../../src/app"
import type { EngineEvent } from "../../src/protocol"
import { createInitialState, type RottweilerState, type ToolProjection } from "../../src/state"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { createStreamingTail } from "../../src/state/model"
import {
  type RottweilerTheme
} from "../../src/theme"

export const initialEvent = {
  type: "text_delta",
  meta: {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-tui-test",
    sequence_id: "1",
    emitted_at: "2026-01-01T00:00:00Z",
  },
  turn_id: "turn-tui-test",
  text: "hello",
} satisfies EngineEvent

export class ManualPresentationFrame implements PresentationFrameScheduler {
  #next = 0
  readonly callbacks = new Map<number, () => void>()
  readonly delays: number[] = []
  scheduled = 0

  schedule(callback: () => void, delayMs: number): number {
    const handle = ++this.#next
    this.callbacks.set(handle, callback)
    this.delays.push(delayMs)
    this.scheduled += 1
    return handle
  }

  cancel(handle: unknown): void {
    if (typeof handle === "number") this.callbacks.delete(handle)
  }

  flush(): void {
    const callbacks = [...this.callbacks.values()]
    this.callbacks.clear()
    for (const callback of callbacks) callback()
  }
}

export function rgba(hex: string): [number, number, number, number] {
  return [
    Number.parseInt(hex.slice(1, 3), 16),
    Number.parseInt(hex.slice(3, 5), 16),
    Number.parseInt(hex.slice(5, 7), 16),
    255,
  ]
}

export function expectCoherentTheme(app: ReturnType<typeof createRottweilerApp>, theme: RottweilerTheme) {
  expect(app.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.main.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.transcript.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.contextPanel.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.composer.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.reviewPanel.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.reviewPanel.rightRail.backgroundColor.toInts()).toEqual(rgba(theme.backgroundPanel))
  expect(app.interactionPanel.backgroundColor.toInts()).toEqual(rgba(theme.backgroundElement))
  expect(app.picker.backgroundColor.toInts()).toEqual(rgba(theme.backgroundElement))
  expect(app.statusLine.bg.toInts()).toEqual(rgba(theme.backgroundPanel))
}

export function completeTransportReconnect(app: ReturnType<typeof createRottweilerApp>): void {
  app.setState({
    ...app.state,
    connection: { phase: "reconnecting", attempt: 1, error: null, gap: null },
  })
  app.setState({
    ...app.state,
    connection: { phase: "connected", attempt: 1, error: null, gap: null },
  })
}

export function visionCapableState() {
  return {
    ...createInitialState(),
    model: "openai/vision",
    provider: "openai",
    models: [{
      id: "openai/vision",
      displayName: "Vision",
      provider: "openai",
      aliases: ["vision"],
      current: true,
      available: true,
      status: null,
      vision: true,
      thinking: true,
      toolCalling: true,
    }],
  }
}

export function toolsAppState(): RottweilerState {
  const tools = Object.fromEntries(Array.from({ length: 8 }, (_, index) => {
    const item: ToolProjection = {
      toolCallId: `tools-${index}`,
      invocationId: `tools-${index}`,
      turnId: "turn-tools",
      name: "bash",
      args: { command: `bun test tools-${index}` },
      status: "running",
      capabilities: ["execute"],
      rationale: null,
      diff: null,
      chunks: toolOutputBuffer([{
        stream: "stdout",
        chunk: Array.from({ length: 8 }, (__, line) => `tools-${index}-${line + 1}`).join("\n"),
      }]),
      output: null,
      isError: null,
      callIndex: index,
      timing: {
        kind: "open",
        startedAtMs: Date.parse("2026-01-01T12:00:00.000Z"),
        lastObservedAtMs: Date.parse("2026-01-01T12:00:10.000Z"),
      },
    }
    return [item.toolCallId, item]
  }))
  return {
    ...createInitialState(),
    transcript: Array.from({ length: 24 }, (_, index) => ({
      sequenceId: `${index + 1}`,
      agentTurn: `history-${index}`,
      turn: {
        role: "assistant" as const,
        blocks: [{ type: "text" as const, text: `Historical response ${index}\nsecond line` }],
        meta: { synthetic: false, summary: false },
      },
    })),
    streamingTail: createStreamingTail({
      turnId: "turn-tools",
      text: "",
      thinking: "",
      citations: [],
      toolCallIds: Object.keys(tools),
      finished: null,
    }),
    turns: {
      "turn-tools": {
        turnId: "turn-tools",
        status: "running",
        usage: null,
        cost: null,
        timing: {
          kind: "open",
          startedAtMs: Date.parse("2026-01-01T12:00:00.000Z"),
          lastObservedAtMs: Date.parse("2026-01-01T12:00:10.000Z"),
        },
      },
    },
    tools,
    queuedMessages: [
      { position: "1", content: "Run the focused suite" },
      { position: "2", content: "Inspect the raster" },
    ],
  }
}
