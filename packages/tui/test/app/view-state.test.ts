import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import { colorContrast, pickerSelectionColors } from "../../src/components/picker"
import type { ClientCommand, EngineEvent } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import {
  kennelTheme,
  systemThemeFor,
  themeCatalogFor
} from "../../src/theme"
import { emptySessionReader } from "../fixtures/history"
import { initialEvent, ManualPresentationFrame } from "./fixtures"

describe("Rottweiler view-state", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("keeps only the newest workspace-status and review query responses", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    let request = 0
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      requestId: () => `projection-${++request}`,
      initialState: {
        ...createInitialState(),
        workspaceStatus: {
          workspaceName: "Rottweiler",
          branch: "main",
          changedPaths: ["src/first.rs", "src/second.rs"],
          truncated: false,
        },
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    app.handleEvent({
      type: "user_shell_state_changed",
      meta: { ...initialEvent.meta, sequence_id: "status-1" },
      shell_id: "shell-status",
      active: false,
      status: 0,
      captured_output: "",
    })
    app.handleEvent({
      type: "command_finished",
      meta: { ...initialEvent.meta, sequence_id: "status-2" },
      name: "fixture",
      message: "done",
      unrestorable_paths: [],
    })
    const statusRequests = commands.filter((command) => command.type === "get_workspace_status")
    expect(statusRequests).toHaveLength(2)
    const oldStatusRequest = statusRequests[0]!.meta.request_id
    const newStatusRequest = statusRequests[1]!.meta.request_id
    const status = (requestId: string, path: string): EngineEvent => ({
      type: "workspace_status_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: requestId,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      status: { workspace_name: "Rottweiler", branch: "main", changed_paths: [path], truncated: false },
    })
    app.handleEvent(status(oldStatusRequest, "src/stale.rs"))
    expect(app.state.workspaceStatus?.changedPaths).toEqual(["src/first.rs", "src/second.rs"])
    app.handleEvent(status(newStatusRequest, "src/current.rs"))
    expect(app.state.workspaceStatus?.changedPaths).toEqual(["src/current.rs"])

    app.openReview()
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    app.openReview()
    const reviewRequests = commands.filter((command) => command.type === "get_session_review")
    expect(reviewRequests).toHaveLength(2)
    const oldReviewRequest = reviewRequests[0]!.meta.request_id
    const newReviewRequest = reviewRequests[1]!.meta.request_id
    const review = (requestId: string, path: string): EngineEvent => ({
      type: "session_review_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: requestId,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      review: {
        session_id: "session-local",
        files: [{
          path,
          unified_diff: `--- a/${path}\n+++ b/${path}\n-old\n+new\n`,
          status: "pending",
          truncated: false,
          unrestorable_reason: null,
          original_hash: "old",
          current_hash: "new",
        }],
      },
    })
    app.handleEvent(review(oldReviewRequest, "src/first.rs"))
    expect(app.state.review).toBeNull()
    app.handleEvent(review(newReviewRequest, "src/second.rs"))
    expect(app.state.review?.files[0]?.path).toBe("src/second.rs")
    expect(app.reviewPanel.diff.diff).toContain("+new")
  })

  test("keeps context and git status live after a completed turn", async () => {
    const setup = await createTestRenderer({ width: 100, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const presentationFrame = new ManualPresentationFrame()
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      presentationFrame,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.handleEvent({
      type: "workspace_status_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "initial-workspace",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      status: {
        workspace_name: "Rottweiler",
        branch: "feature/live-status",
        changed_paths: [],
        truncated: false,
      },
    })
    app.handleEvent({
      type: "context_usage_updated",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "10",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      turn_id: "1",
      used_tokens: "500",
      usable_tokens: "1000",
      reserved_tokens: "100",
      context_window_known: true,
      stable_prefix_hash: "not-presented",
      cache_hit_basis_points: 0,
      estimated_input_tokens: "500",
      provider_input_tokens: "500",
      correction_millionths: "1000000",
    })
    presentationFrame.flush()
    await setup.renderOnce()
    expect(app.statusLine.plainText).toContain("ctx 50%")
    expect(app.statusLine.plainText).toContain("feature/live-status")

    app.handleEvent({
      type: "turn_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "11",
        emitted_at: "2026-01-01T00:00:02Z",
      },
      turn_id: "1",
      status: "completed",
      usage: {
        input_tokens: "500",
        output_tokens: "20",
        cache_read_tokens: "0",
        cache_write_tokens: "0",
        reasoning_tokens: "0",
      },
      cost: { kind: "unavailable", reason: "fixture" },
    })
    expect(commands.slice(-3).map((command) => command.type)).toEqual([
      "get_workspace_status",
      "get_context",
      "get_cost",
    ])
  })

  test("preserves picker selection and visible window across unrelated state events", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        commands: Array.from({ length: 20 }, (_, index) => ({
          name: `command-${index}`,
          description: `Command ${index}`,
          usage: `/command-${index}`,
        })),
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    expect(app.commandPalette.itemIds).toContain("slash.command-15")
    app.commandPalette.selectById("slash.command-15")
    const commandIndex = app.commandPalette.selectedRowIndex
    await setup.renderOnce()

    app.handleEvent({
      ...initialEvent,
      meta: { ...initialEvent.meta, sequence_id: "selection-refresh" },
      text: "unrelated state refresh",
    })
    await setup.renderOnce()

    expect(app.commandPalette.selectedId).toBe("slash.command-15")
    expect(app.commandPalette.selectedRowIndex).toBe(commandIndex)
    expect(setup.captureCharFrame()).toContain("/command-15")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.commandPalette.visible).toBeFalse()
  })

  test("preserves command palette query, selection, and viewport across a theme rebuild", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      theme: systemThemeFor("dark"),
      initialState: {
        ...createInitialState(),
        commands: Array.from({ length: 30 }, (_, index) => ({
          name: `command-${index}`,
          description: `Command ${index}`,
          usage: `/command-${index}`,
        })),
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.mockInput.typeText("command")
    app.commandPalette.selectById("slash.command-15")
    const original = app.commandPalette
    const offset = app.commandPalette.scrollOffset

    app.setSystemTheme(systemThemeFor("light"))
    await setup.renderOnce()

    expect(app.commandPalette).not.toBe(original)
    expect(app.commandPalette.visible).toBeTrue()
    expect(app.commandPalette.input.value).toBe("command")
    expect(app.commandPalette.selectedId).toBe("slash.command-15")
    expect(app.commandPalette.scrollOffset).toBe(offset)
  })

  test("keeps picker selection readable and distinct in every bundled theme", () => {
    for (const mode of ["dark", "light"] as const) {
      for (const theme of themeCatalogFor(mode)) {
        const selected = pickerSelectionColors(theme)
        expect(colorContrast(selected.foreground, selected.background), theme.name).toBeGreaterThanOrEqual(4.5)
        expect(colorContrast(selected.background, theme.backgroundElement), theme.name).toBeGreaterThanOrEqual(1.4)
      }
    }
    const transparentSelection = pickerSelectionColors({
      ...kennelTheme,
      selectedListItemText: "#00000000",
    })
    expect(transparentSelection.foreground).toMatch(/^#[0-9A-Fa-f]{6}$/)
    expect(colorContrast(transparentSelection.foreground, transparentSelection.background)).toBeGreaterThanOrEqual(4.5)
  })
})
