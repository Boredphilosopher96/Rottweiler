import { todoState } from "../fixtures/todos"
import {
  createTestRenderer,
  setRendererCapabilities,
  type TestRenderer
} from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { ContextPanelRenderable, fuzzyScore, ImageAttachmentRenderable } from "../../src/components"
import {
  PROTOCOL_VERSION,
  type ClientCommand
} from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { kennelTheme } from "../../src/theme"
import { emptySessionReader } from "../fixtures/history"

describe("sidebar components", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("shows active MCPs with todos and changed files in the sidebar and opens exact paths", async () => {
    const setup = await createTestRenderer({ width: 52, height: 30, useThread: false })
    renderer = setup.renderer
    const opened: string[] = []
    const panel = new ContextPanelRenderable(renderer, kennelTheme, {
      onOpenDiff: (path) => opened.push(path),
    })
    renderer.root.add(panel)
    panel.update({
      ...createInitialState(),
      todos: todoState([
        { id: "audit", content: "Audit interactions", status: "in_progress" },
        { id: "tests", content: "Add regression tests", status: "pending" },
      ]),
      mcpServers: [
        { name: "docs", enabled: true, approved: true, state: { type: "ready" }, tool_count: 4, resource_count: 0, prompt_count: 0 },
        { name: "search", enabled: true, approved: true, state: { type: "connecting" }, tool_count: 0, resource_count: 0, prompt_count: 0 },
        { name: "disabled", enabled: false, approved: false, state: { type: "disabled" }, tool_count: 0, resource_count: 0, prompt_count: 0 },
        { name: "failed", enabled: true, approved: true, state: { type: "failed", message: "offline" }, tool_count: 0, resource_count: 0, prompt_count: 0 },
      ],
      runtimeServices: [
        { kind: "lsp", name: "rust-analyzer" },
        { kind: "formatter", name: "rustfmt" },
        { kind: "linter", name: "clippy-driver" },
      ],
      workspaceStatus: {
        workspaceName: "Rottweiler",
        branch: "main",
        changedPaths: ["src/from-status.rs", "src/shared.rs"],
        truncated: false,
      },
      review: {
        sessionId: "session-sidebar",
        files: [
          {
            path: "src/from-review.rs",
            unifiedDiff: "+review",
            status: "pending",
            truncated: false,
            unrestorableReason: null,
            originalHash: "old",
            currentHash: "new",
          },
          {
            path: "src/shared.rs",
            unifiedDiff: "+shared",
            status: "pending",
            truncated: false,
            unrestorableReason: null,
            originalHash: "old",
            currentHash: "new",
          },
        ],
      },
    })
    await setup.renderOnce()

    expect(panel.todos.options.map((option) => option.value)).toEqual(["audit", "tests"])
    expect(panel.changedFiles.options.map((option) => option.value)).toEqual([
      "src/shared.rs",
      "src/from-status.rs",
    ])
    expect(panel.mcps.options.map((option) => option.value)).toEqual(["docs", "search"])
    expect(panel.runtimeServices.options.map((option) => option.value)).toEqual([
      "lsp:rust-analyzer",
      "formatter:rustfmt",
      "linter:clippy-driver",
    ])
    const frame = setup.captureCharFrame()
    expect(frame).toContain("TASKS")
    expect(frame).toContain("MCP")
    expect(frame).toContain("docs · 4 tools")
    expect(frame).not.toContain("disabled")
    expect(frame).not.toContain("failed")
    expect(frame).toContain("SERVICES")
    expect(frame).toContain("LSP · rust-analyzer")
    expect(frame).toContain("CHANGED")
    expect(frame).not.toContain("context")

    panel.changedFiles.focus()
    panel.changedFiles.setSelectedIndex(0)
    setup.mockInput.pressEnter()
    expect(opened).toEqual(["src/shared.rs"])

    await setup.mockMouse.click(panel.changedFiles.x + 2, panel.changedFiles.y + 1)
    expect(opened).toEqual(["src/shared.rs", "src/from-status.rs"])
  })

  test("keeps changed files visible when MCP and runtime sections fill a short sidebar", async () => {
    const setup = await createTestRenderer({ width: 52, height: 18, useThread: false })
    renderer = setup.renderer
    const panel = new ContextPanelRenderable(renderer, kennelTheme, {})
    renderer.root.add(panel)
    panel.update({
      ...createInitialState(),
      todos: todoState([{ id: "todo", content: "Keep the viewport bounded", status: "pending" }]),
      mcpServers: Array.from({ length: 4 }, (_, index) => ({
        name: `mcp-${index}`,
        enabled: true,
        approved: true,
        state: { type: "ready" as const },
        tool_count: 1,
        resource_count: 0,
        prompt_count: 0,
      })),
      runtimeServices: Array.from({ length: 5 }, (_, index) => ({
        kind: index === 0 ? "lsp" as const : "linter" as const,
        name: `service-${index}`,
      })),
      workspaceStatus: {
        workspaceName: "Rottweiler",
        branch: "main",
        changedPaths: ["src/changed.rs"],
        truncated: false,
      },
    })
    await setup.renderOnce()

    const frame = setup.captureCharFrame()
    expect(frame).toContain("CHANGED")
    expect(frame).toContain("src/changed.rs")
    expect(frame.indexOf("CHANGED")).toBeGreaterThan(frame.indexOf("service-4"))
  })

  test("opens the exact retained diff from the default changed-files sidebar", async () => {
    const setup = await createTestRenderer({ width: 112, height: 30, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      requestId: () => "workspace-diff-request",
      onCommand: () => ({ type: "accepted" }),
      initialState: {
        ...createInitialState(),
        workspaceStatus: {
          workspaceName: "Rottweiler",
          branch: "main",
          changedPaths: ["src/first.rs", "src/exact.rs"],
          truncated: false,
        },
        review: {
          sessionId: "sidebar-diff",
          files: [
            {
              path: "src/first.rs",
              unifiedDiff: "--- a/src/first.rs\n+++ b/src/first.rs\n-old\n+first\n",
              status: "pending",
              truncated: false,
              unrestorableReason: null,
              originalHash: "old-first",
              currentHash: "new-first",
            },
            {
              path: "src/exact.rs",
              unifiedDiff: "--- a/src/exact.rs\n+++ b/src/exact.rs\n-old\n+exact\n",
              status: "pending",
              truncated: false,
              unrestorableReason: null,
              originalHash: "old-exact",
              currentHash: "new-exact",
            },
          ],
        },
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()
    expect(app.reviewPanel.visible).toBeFalse()

    app.contextPanel.changedFiles.focus()
    app.contextPanel.changedFiles.setSelectedIndex(1)
    setup.mockInput.pressEnter()
    expect(app.reviewPanel.visible).toBeTrue()
    app.handleEvent({
      type: "workspace_diff_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "workspace-diff-request",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      diff: {
        path: "src/exact.rs",
        unified_diff: "--- a/src/exact.rs\n+++ b/src/exact.rs\n@@ -1,9 +1,9 @@\n-old\n+exact\n",
        truncated: false,
        binary: false,
      },
    })
    expect(app.reviewPanel.diff.diff).toContain("+exact")
    expect(app.reviewPanel.diff.diff).toContain("@@ -1,1 +1,1 @@")
    expect(app.reviewPanel.diff.filetype).toBe("rust")
    expect(app.reviewPanel.diff.diff).not.toContain("Error parsing diff")
    expect(app.composer.visible).toBeTrue()

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.reviewPanel.visible).toBeFalse()
    expect(app.composer.visible).toBeTrue()
    expect(app.state.review?.files).toHaveLength(2)
  })

  test("searches sessions remotely while preserving instant local fuzzy filtering", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        sessions: [
          {
            sessionId: "session-rottweiler",
            workspaceName: "Rottweiler",
            model: "fast",
            driverClientId: null,
            shellActive: false,
          },
        ],
      },
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
    app.openSessionPicker()
    setup.mockInput.typeText("rott")
    await Bun.sleep(100)

    expect(commands[0]).toMatchObject({ type: "list_sessions" })
    expect(commands.at(-1)).toMatchObject({ type: "search_sessions", query: "rott", limit: 100 })
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "session-rottweiler",
      "sessions.new",
    ])
  })

  test("fuzzy matching is ordered and image fallback is capability gated", async () => {
    expect(fuzzyScore("ctx", "context inspect")).toBeGreaterThan(
      fuzzyScore("ctx", "long command text x") ?? -1,
    )
    expect(fuzzyScore("zzz", "context inspect")).toBeNull()

    const setup = await createTestRenderer({ width: 50, height: 8, useThread: false })
    renderer = setup.renderer
    setRendererCapabilities(renderer, { kitty_graphics: false, sixel: false })
    const image = new ImageAttachmentRenderable(renderer, kennelTheme, {
      name: "screen.png",
      media_type: "image/png",
      data: { type: "inline_base64", data: "AA==" },
    })
    renderer.root.add(image)
    await setup.renderOnce()
    expect(image.height).toBe(2)
    expect(setup.captureCharFrame()).toContain("screen.png")
  })
})
