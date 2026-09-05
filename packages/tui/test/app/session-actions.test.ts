import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { homedir } from "node:os"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptyHistoryReader } from "../fixtures/history"

describe("Rottweiler session-actions", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("manages queued messages from the Conversation palette and refreshes after removal", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        queuedMessages: [
          { position: "1", content: "Remove this instruction\nwith hidden details" },
          { position: "2", content: "Keep this instruction" },
        ],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    const paletteOptions = app.commandPalette.itemIds
    const planIndex = paletteOptions.indexOf("plan.show")
    const queueIndex = paletteOptions.indexOf("queue.manage")
    const costIndex = paletteOptions.indexOf("cost.show")
    expect(queueIndex).toBe(planIndex + 1)
    expect(costIndex).toBe(queueIndex + 1)
    app.commandPalette.selectById("queue.manage")
    expect(app.commandPalette.detail.plainText).toContain("Manage queued messages")
    expect(app.commandPalette.detail.plainText).toContain("Review, remove, or clear queued messages")
    app.commandPalette.activateSelected()

    expect(app.picker.title).toContain("Queued messages")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "Remove this instruction",
      "Keep this instruction",
      "Clear all queued messages",
    ])
    expect(app.picker.select.options.map((option) => option.description)).toEqual([
      "queued",
      "queued",
      "Remove every queued message",
    ])

    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "remove_queued_message",
      position: "1",
    }))
    expect(app.picker.visible).toBeTrue()

    app.handleEvent({
      type: "queued_message_removed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tui-test",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      position: "1",
    })
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "Keep this instruction",
    ])

    app.handleEvent({
      type: "message_queued",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tui-test",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:02Z",
      },
      position: "3",
      content: "Another queued instruction",
      attachments: [],
    })
    const clearIndex = app.picker.select.options.findIndex(
      (option) => option.value === "queued.messages.clear",
    )
    expect(clearIndex).toBeGreaterThanOrEqual(0)
    app.picker.select.setSelectedIndex(clearIndex)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.picker.visible).toBeFalse()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "clear_queued_messages",
    }))
  })

  test("shows an empty queued-message status without actionable rows", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openQueuedMessagesPicker()
    expect(app.picker.status.plainText).toContain("No queued messages")
    expect(app.picker.status.visible).toBeTrue()
    expect(app.picker.select.visible).toBeFalse()
    expect(app.picker.select.options).toHaveLength(0)
    app.picker.select.selectCurrent()
    expect(emitted.filter((command) =>
      command.type === "remove_queued_message" || command.type === "clear_queued_messages"
    )).toEqual([])
  })

  test("does not open queued-message controls during historical replay", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      replaySessionId: "historical-queue",
      initialState: {
        ...createInitialState(),
        queuedMessages: [{ position: "1", content: "Historical queued instruction" }],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openQueuedMessagesPicker()
    expect(app.picker.visible).toBeFalse()
    expect(emitted.filter((command) =>
      command.type === "remove_queued_message" || command.type === "clear_queued_messages"
    )).toEqual([])
  })

  test("exports the live session through the Conversation palette picker and path prompt", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      requestId: () => `export-request-${request++}`,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    const paletteOptions = app.commandPalette.itemIds
    const reviewIndex = paletteOptions.indexOf("review.open")
    const exportIndex = paletteOptions.indexOf("session.export")
    expect(exportIndex).toBe(reviewIndex + 1)
    app.commandPalette.selectById("session.export")
    expect(app.commandPalette.detail.plainText).toContain("Export session")
    expect(app.commandPalette.detail.plainText).toContain("Save this session's transcript to a file")
    app.commandPalette.activateSelected()

    expect(app.picker.title).toContain("Export session")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "Markdown",
      "HTML",
      "JSON",
    ])
    expect(app.picker.select.options.map((option) => option.description)).toEqual([
      "Readable text",
      "Formatted for a browser",
      "Structured data",
    ])
    app.picker.select.setSelectedIndex(1)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Save to path, e.g. ~/transcript.md")
    expect(app.picker.input.placeholder).toBe("~/rottweiler-export.html")

    await setup.mockInput.typeText("~/rottweiler-session-export.html")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    const exportCommand = emitted.find((command) => command.type === "export_session")
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "export_session",
      session_id: "session-local",
      format: "html",
      output_path: `${homedir()}/rottweiler-session-export.html`,
      force: false,
    }))

    app.handleEvent({
      type: "session_exported",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: exportCommand?.meta.request_id ?? "missing-export-request",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      output_path: "/private/tmp/rottweiler-session-export.html",
    })
    expect(app.banner.visible).toBeTrue()
    expect(app.banner.plainText).toBe(
      "Exported to /private/tmp/rottweiler-session-export.html",
    )
  })

  test("surfaces export failures and retries an existing file with atomic force replacement", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      requestId: () => `export-${request++}`,
      onCommand(command) {
        emitted.push(command)
        if (command.type === "export_session" && !command.force) {
          return {
            type: "rejected",
            error: {
              category: "protocol",
              code: "host_query_failure",
              message: "export output already exists; pass --force to replace it",
              retryable: false,
            },
          }
        }
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openExportSessionPicker()
    app.picker.select.selectCurrent()
    await setup.mockInput.typeText("/tmp/existing-transcript.md")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)

    expect(app.state.errors.at(-1)).toMatchObject({
      code: "host_query_failure",
      message: "export output already exists; pass --force to replace it",
    })
    expect(app.picker.title).toContain("Overwrite existing file?")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "Overwrite",
      "Cancel",
    ])
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(emitted.filter((command) => command.type === "export_session")).toEqual([
      expect.objectContaining({
        type: "export_session",
        output_path: "/tmp/existing-transcript.md",
        force: false,
      }),
      expect.objectContaining({
        type: "export_session",
        output_path: "/tmp/existing-transcript.md",
        force: true,
      }),
    ])
  })

  test("does not open or send session export controls during historical replay", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      replaySessionId: "historical-export",
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openExportSessionPicker()
    expect(app.picker.visible).toBeFalse()
    expect(emitted.filter((command) => command.type === "export_session")).toEqual([])
  })

  test("shows ordered live workspace roots from the Workspace palette", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        workspaceRoots: {
          generation: "2",
          effectiveFromTurn: "5",
          roots: ["/workspace/primary", "/workspace/additional"],
        },
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    const paletteOptions = app.commandPalette.itemIds
    const addIndex = paletteOptions.indexOf("workspace.add")
    const rootsIndex = paletteOptions.indexOf("workspace.roots")
    const trustIndex = paletteOptions.indexOf("trust.manage")
    expect(rootsIndex).toBe(addIndex + 1)
    expect(trustIndex).toBe(rootsIndex + 1)
    app.commandPalette.selectById("workspace.roots")
    expect(app.commandPalette.detail.plainText).toContain("Workspace roots")
    expect(app.commandPalette.detail.plainText).toContain("See every live workspace root")
    app.commandPalette.activateSelected()

    expect(app.picker.title).toContain("Workspace roots")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "/workspace/primary",
      "/workspace/additional",
    ])
    expect(app.picker.select.options.map((option) => option.description)).toEqual([
      "primary",
      "additional",
    ])
    app.picker.select.selectCurrent()
    expect(app.picker.visible).toBeFalse()
  })

  test("shows workspace-root loading state before the live inventory arrives", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { historyReader: emptyHistoryReader })
    renderer.root.add(app)

    app.openWorkspaceRootsPicker()

    expect(app.picker.title).toContain("Workspace roots")
    expect(app.picker.status.plainText).toContain("Loading workspace roots")
    expect(app.picker.status.visible).toBeTrue()
    expect(app.picker.select.visible).toBeFalse()
    expect(app.picker.select.options).toHaveLength(0)
  })
})
