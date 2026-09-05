import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { emptyHistoryReader } from "../fixtures/history"

describe("Rottweiler timeline", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("opens the conversation timeline for /rewind without an argument", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        commands: [{ name: "rewind", description: "Rewind the conversation", usage: "/rewind" }],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    await setup.mockInput.typeText("/rew")
    expect(app.picker.select.getSelectedOption()?.value).toBe("rewind")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)

    expect(app.composer.value).toBe("")
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.title).toContain("Conversation timeline")
    expect(app.picker.status.plainText).toContain("No completed user turns")
    expect(emitted.some((command) => command.type === "send_message")).toBeFalse()
  })

  test("builds newest-first timeline rows from completed user transcript entries", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        transcript: [{
          sequenceId: "10",
          agentTurn: "2",
          turn: {
            role: "user",
            blocks: [{ type: "text", text: "Older request\nwith more detail" }],
            meta: { synthetic: false, summary: false },
          },
        }, {
          sequenceId: "20",
          agentTurn: "5",
          turn: {
            role: "user",
            blocks: [{
              type: "text",
              text: "Newest request has a deliberately long first line that must be bounded for the picker row",
            }],
            meta: { synthetic: false, summary: false },
          },
        }],
        tools: {
          edit: {
            toolCallId: "edit",
            invocationId: "edit",
            turnId: "5",
            name: "edit",
            args: { path: "src/app.ts" },
            status: "finished",
            capabilities: ["write_filesystem"],
            rationale: "Update the picker",
            diff: {
              proposal_id: "proposal",
              path: "src/app.ts",
              unified_diff: "+timeline\n",
              arguments_hash: "arguments",
              base_hash: "base",
              diff_hash: "diff",
              truncated: false,
            },
            chunks: toolOutputBuffer([]),
            output: { type: "text", text: "done" },
            isError: false,
            callIndex: 0,
            timing: { kind: "unknown" },
          },
        },
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    app.commandPalette.selectById("rewind.run")
    app.commandPalette.activateSelected()

    expect(app.picker.title).toContain("Conversation timeline")
    const rows = app.picker.select.options.filter(
      (option) => String(option.value).startsWith("timeline.turn."),
    )
    expect(rows.map((row) => row.name)).toEqual([
      "Newest request has a deliberately long first line that must be …",
      "Older request",
    ])
    expect(rows[0]?.description).toBe("turn 5 · 1 tool · 1 edit")
    expect(rows[1]?.description).toBe("turn 2")
  })

  test("fills and focuses the composer after an edit-and-resend rewind completes", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const original = "Keep this exact text\nincluding its second line."
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      requestId: () => "rewind-edit-request",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        transcript: [{
          sequenceId: "1",
          agentTurn: "3",
          turn: {
            role: "user",
            blocks: [
              { type: "text", text: original },
              { type: "image", media_type: "image/png", data: { type: "url", url: "https://example.invalid/image.png" } },
            ],
            meta: { synthetic: false, summary: false },
          },
        }],
      },
    })
    renderer.root.add(app)
    app.openTimelinePicker()
    app.picker.select.selectCurrent()
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "Edit and resend",
      "Retry",
      "Rewind only",
    ])
    app.picker.select.selectCurrent()
    await Bun.sleep(0)

    expect(commands).toContainEqual(expect.objectContaining({
      type: "send_message",
      content: "/rewind 2",
      attachments: [],
    }))
    app.handleEvent({
      type: "conversation_rewound",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
        caused_by: "rewind-edit-request",
      },
      to_agent_turn: "2",
      operation_id: "rewind-edit-operation",
      unrestorable_paths: [],
    })

    expect(app.composer.value).toBe(original)
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
    expect(app.banner.plainText).toBe("attachments from the original message are not restored")
  })

  test("retries the exact original text only after the rewind event", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const original = "  preserve whitespace\nand newlines exactly  "
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      requestId: () => `rewind-retry-${request++}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        transcript: [{
          sequenceId: "4",
          agentTurn: "4",
          turn: {
            role: "user",
            blocks: [{ type: "text", text: original }],
            meta: { synthetic: false, summary: false },
          },
        }],
      },
    })
    renderer.root.add(app)
    app.openTimelinePicker()
    app.picker.select.selectCurrent()
    const retry = app.picker.select.options.findIndex(
      (option) => option.value === "timeline.action.retry",
    )
    app.picker.select.setSelectedIndex(retry)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)

    expect(commands.filter((command) => command.type === "send_message")).toEqual([
      expect.objectContaining({ content: "/rewind 3", attachments: [] }),
    ])
    app.handleEvent({
      type: "conversation_rewound",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
        caused_by: "rewind-retry-0",
      },
      to_agent_turn: "3",
      operation_id: "rewind-retry-operation",
      unrestorable_paths: [],
    })
    await Bun.sleep(0)

    expect(commands.filter((command) => command.type === "send_message")).toEqual([
      expect.objectContaining({ content: "/rewind 3", attachments: [] }),
      expect.objectContaining({ content: original, attachments: [] }),
    ])
  })

  test("clears a pending edit intent when the rewind request is rejected", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      requestId: () => "rewind-rejected-request",
      onCommand() {
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        transcript: [{
          sequenceId: "3",
          agentTurn: "3",
          turn: {
            role: "user",
            blocks: [{ type: "text", text: "Do not restore after rejection" }],
            meta: { synthetic: false, summary: false },
          },
        }],
      },
    })
    renderer.root.add(app)
    app.openTimelinePicker()
    app.picker.select.selectCurrent()
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    app.handleEvent({
      type: "command_acknowledged",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "rewind-rejected-request",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      outcome: {
        type: "rejected",
        error: {
          category: "protocol",
          code: "turn_running",
          message: "interrupt the active turn before rewinding",
          retryable: false,
        },
      },
    })
    app.handleEvent({
      type: "conversation_rewound",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:01Z",
        caused_by: "rewind-rejected-request",
      },
      to_agent_turn: "2",
      operation_id: "late-rewind-operation",
      unrestorable_paths: [],
    })

    expect(app.state.errors.at(-1)?.code).toBe("turn_running")
    expect(app.composer.value).toBe("")
  })

  test("shows replay timelines as read-only rows with no actions", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      replaySessionId: "historical-timeline",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        transcript: [{
          sequenceId: "1",
          agentTurn: "2",
          turn: {
            role: "user",
            blocks: [{ type: "text", text: "Historical request" }],
            meta: { synthetic: false, summary: false },
          },
        }],
      },
    })
    renderer.root.add(app)
    app.openTimelinePicker()

    expect(app.picker.title).toContain("Conversation timeline")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "read-only session",
      "Historical request",
    ])
    expect(app.picker.select.options[1]?.description).toBe("turn 2 · read-only")
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Conversation timeline")
    expect(commands.filter((command) => command.type === "send_message")).toEqual([])
  })
})
