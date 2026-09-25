import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { homedir } from "node:os"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"
import { options, select, statusText } from "../picker-screen"

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
      sessionReader: emptySessionReader,
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
    const compactIndex = paletteOptions.indexOf("cmd.compact")
    const queueIndex = paletteOptions.indexOf("cmd.queue")
    const usageIndex = paletteOptions.indexOf("cmd.usage")
    expect(queueIndex).toBe(compactIndex + 1)
    expect(usageIndex).toBeGreaterThan(queueIndex)
    app.commandPalette.selectById("cmd.queue")
    expect(app.commandPalette.detail.plainText).toContain("Queued messages")
    expect(app.commandPalette.detail.plainText).toContain("Review, remove, or clear queued messages")
    app.commandPalette.activateSelected()

    expect(app.picker.screenTitle).toContain("QUEUED WORK")
    expect(app.picker.sectionLabels).toEqual(["Messages"])
    expect(options(app.picker).map((option) => option.name)).toEqual([
      "Remove this instruction",
      "Keep this instruction",
    ])
    expect(app.picker.items.map((item) => item.hint)).toEqual(["#1", "#2"])
    expect(app.picker.footer.plainText).toBe("⏎ remove · ctrl+l clear all · esc close")

    select(app.picker, 0)
    app.picker.activateSelected()
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
    expect(options(app.picker).map((option) => option.name)).toEqual([
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
    setup.mockInput.pressKey("l", { ctrl: true })
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
      sessionReader: emptySessionReader,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openQueuedMessagesPicker()
    expect(statusText(app.picker)).toContain("Nothing is queued")
    expect((app.picker.mode === "status")).toBeTrue()
    expect((app.picker.mode === "list")).toBeFalse()
    expect(options(app.picker)).toHaveLength(0)
    app.picker.activateSelected()
    expect(emitted.filter((command) =>
      command.type === "remove_queued_message" || command.type === "clear_queued_messages"
    )).toEqual([])
  })

  test("does not open queued-message controls during historical replay", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
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

  test("exports the live session from the Sessions screen through the format picker and path prompt", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      requestId: () => `export-request-${request++}`,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    app.commandPalette.selectById("cmd.resume")
    expect(app.commandPalette.detail.plainText).toContain("Resume, rename, or export a session")
    app.commandPalette.activateSelected()
    const list = emitted.findLast((command) => command.type === "list_sessions")
    if (list?.type !== "list_sessions") throw new Error("missing session list")
    app.handleEvent({ type: "sessions_listed", meta: { ...list.meta, emitted_at: "2026-01-01T00:00:00Z" }, sessions: [] })
    expect(app.picker.footer.plainText).toContain("ctrl+x export")
    setup.mockInput.pressKey("x", { ctrl: true })

    expect(app.picker.screenTitle).toContain("EXPORT SESSION")
    expect(options(app.picker).map((option) => option.name)).toEqual([
      "Markdown",
      "HTML",
      "JSON",
    ])
    expect(options(app.picker).map((option) => option.description)).toEqual([
      "Readable text",
      "Formatted for a browser",
      "Structured data",
    ])
    select(app.picker, 1)
    app.picker.activateSelected()
    expect(app.picker.screenTitle).toBe("EXPORT › HTML")
    expect(statusText(app.picker)).toContain("Path for the HTML transcript")
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
      sessionReader: emptySessionReader,
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
    app.picker.activateSelected()
    await setup.mockInput.typeText("/tmp/existing-transcript.md")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)

    expect(app.state.errors.at(-1)).toMatchObject({
      code: "host_query_failure",
      message: "export output already exists; pass --force to replace it",
    })
    expect(app.picker.screenTitle).toContain("File exists")
    expect(options(app.picker).map((option) => option.name)).toEqual([
      "Overwrite",
      "Keep existing file",
    ])
    expect(app.picker.selectedItem?.label).toBe("Keep existing file")
    app.picker.selectById("export.overwrite.confirm")
    app.picker.activateSelected()
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
      sessionReader: emptySessionReader,
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
      sessionReader: emptySessionReader,
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
    const reviewIndex = paletteOptions.indexOf("cmd.review")
    const dirsIndex = paletteOptions.indexOf("cmd.dirs")
    const permissionsIndex = paletteOptions.indexOf("cmd.permissions")
    expect(dirsIndex).toBe(reviewIndex + 1)
    expect(permissionsIndex).toBeGreaterThan(dirsIndex)
    app.commandPalette.selectById("cmd.dirs")
    expect(app.commandPalette.detail.plainText).toContain("Directories")
    expect(app.commandPalette.detail.plainText).toContain("List workspace roots or add one")
    app.commandPalette.activateSelected()

    expect(app.picker.screenTitle).toContain("WORKSPACE")
    expect(options(app.picker).map((option) => option.name)).toEqual([
      "/workspace/primary",
      "/workspace/additional",
    ])
    expect(app.picker.items.map((item) => item.hint)).toEqual(["primary", "added"])
    expect(app.picker.footer.plainText).toBe("esc close")
  })

  test("shows workspace-root loading state before the live inventory arrives", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)

    app.openWorkspaceRootsPicker()

    expect(app.picker.screenTitle).toContain("WORKSPACE")
    expect(statusText(app.picker)).toContain("Loading workspace directories")
    expect((app.picker.mode === "status")).toBeTrue()
    expect((app.picker.mode === "list")).toBeFalse()
    expect(options(app.picker)).toHaveLength(0)
  })
})
