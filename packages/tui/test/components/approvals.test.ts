import { parseKeypress } from "@opentui/core"
import {
  createTestRenderer,
  type TestRenderer
} from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import {
  PROTOCOL_VERSION,
  type ClientCommand
} from "../../src/protocol"
import { createInitialState, type RottweilerState } from "../../src/state"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { emptySessionReader } from "../fixtures/history"
import { permissionState } from "./fixtures"

describe("approvals components", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("routes diff approval through generated commands", async () => {
    const setup = await createTestRenderer({ width: 112, height: 30, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const state: RottweilerState = {
      ...createInitialState(),
      tools: {
        edit: {
          toolCallId: "edit",
          invocationId: "edit",
          turnId: "1",
          name: "edit",
          args: { path: "src/main.rs" },
          status: "awaiting_approval",
          capabilities: ["write_filesystem"],
          rationale: "Apply change",
          diff: {
            proposal_id: "proposal-hash",
            path: "src/main.rs",
            unified_diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n",
            arguments_hash: "arguments-hash",
            base_hash: "base-hash",
            diff_hash: "diff-hash",
            truncated: false,
          },
          diffSource: null, chunks: toolOutputBuffer([]),
          display: null, source: null,
          isError: null,
          callIndex: 0,
          timing: { kind: "unknown" },
        },
      },
    }
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: state,
      sessionId: "session-components",
      clientId: "client-components",
      requestId: () => "request-components",
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()
    expect(app.interactionPanel.prompt.plainText).toContain("Edit file src/main.rs")
    expect(app.interactionPanel.prompt.plainText).not.toContain("Arguments:")
    app.interactionPanel.select.selectCurrent()

    expect(commands).toContainEqual({
      type: "approve_tool",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-components",
        request_id: "request-components",
      },
      session_id: "session-components",
      tool_call_id: "edit",
      invocation_id: "edit",
      decision: "allow_once",
      binding: {
        proposal_id: "proposal-hash",
        arguments_hash: "arguments-hash",
        base_hash: "base-hash",
        diff_hash: "diff-hash",
      },
    })
    commands.length = 0
    app.setState({
      ...state,
      tools: {
        edit: {
          ...state.tools.edit!,
          diff: { ...state.tools.edit!.diff!, truncated: true },
        },
      },
    })
    await setup.renderOnce()
    expect(app.interactionPanel.select.options.map((option) => option.value)).toEqual(["deny"])
    app.interactionPanel.select.selectCurrent()
    expect(commands).toContainEqual(
      expect.objectContaining({
        type: "approve_tool",
        tool_call_id: "edit",
        invocation_id: "edit",
        decision: "deny",
      }),
    )
  })

  test("commits clicked and focused-keyboard permission choices exactly once", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const tool = {
      toolCallId: "click-approval",
      invocationId: "click-approval",
      turnId: "1",
      name: "write",
      args: { path: "src/clicked.rs" },
      status: "awaiting_approval" as const,
      capabilities: ["write_filesystem" as const],
      rationale: "Create the selected file",
      diff: null,
      diffSource: null, chunks: toolOutputBuffer([]),
      display: null, source: null,
      isError: null,
      callIndex: 0,
      timing: { kind: "unknown" as const },
    }
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: { ...createInitialState(), tools: { [tool.invocationId]: tool } },
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    expect(app.composer.visible).toBeTrue()
    setup.mockInput.pressTab()
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
    await setup.mockInput.typeText("any")
    app.setState({ ...app.state })
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
    expect(commands.filter(command => command.type === "approve_tool")).toHaveLength(0)
    expect(app.composer.value).toBe("any")
    setup.mockInput.pressTab()
    expect(renderer.currentFocusedRenderable).toBe(app.interactionPanel.select)
    setup.mockInput.pressKey("y")
    expect(commands.filter(command => command.type === "approve_tool").at(-1)).toMatchObject({ decision: "allow_once" })
    setup.mockInput.pressKey("a")
    expect(commands.filter(command => command.type === "approve_tool").at(-1)).toMatchObject({ decision: "allow_session" })
    setup.mockInput.pressKey("n")
    expect(commands.filter(command => command.type === "approve_tool").at(-1)).toMatchObject({ decision: "deny" })
    commands.length = 0
    app.interactionPanel.select.setSelectedIndex(0)
    await setup.renderOnce()

    // Each described option occupies two terminal rows. Click the second row's
    // label (Allow session), not the currently highlighted default.
    await setup.mockMouse.click(
      app.interactionPanel.select.x + 4,
      app.interactionPanel.select.y + 2,
    )
    expect(commands.filter((command) => command.type === "approve_tool")).toEqual([
      expect.objectContaining({
        type: "approve_tool",
        tool_call_id: "click-approval",
        invocation_id: "click-approval",
        decision: "allow_session",
      }),
    ])

    commands.length = 0
    app.interactionPanel.select.setSelectedIndex(2)
    app.interactionPanel.select.focus()
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(commands.filter((command) => command.type === "approve_tool")).toEqual([
      expect.objectContaining({
        type: "approve_tool",
        tool_call_id: "click-approval",
        invocation_id: "click-approval",
        decision: "allow_project",
      }),
    ])

    commands.length = 0
    app.interactionPanel.select.setSelectedIndex(0)
    const keypadEnter = parseKeypress("\u001b[57414u", { useKittyKeyboard: true })!
    setup.renderer.keyInput.processParsedKey(keypadEnter)
    await Bun.sleep(0)
    expect(commands.filter((command) => command.type === "approve_tool")).toEqual([
      expect.objectContaining({
        type: "approve_tool",
        tool_call_id: "click-approval",
        invocation_id: "click-approval",
        decision: "allow_once",
      }),
    ])

    commands.length = 0
    const linefeed = parseKeypress("\n", { useKittyKeyboard: true })!
    expect(linefeed.name).toBe("linefeed")
    setup.renderer.keyInput.processParsedKey(linefeed)
    await Bun.sleep(0)
    expect(commands.filter((command) => command.type === "approve_tool")).toEqual([
      expect.objectContaining({
        type: "approve_tool",
        tool_call_id: "click-approval",
        invocation_id: "click-approval",
        decision: "allow_once",
      }),
    ])
  })

  test("offers session-wide tool rules and auto-safe mode as approval escape hatches", async () => {
    const setup = await createTestRenderer({ width: 112, height: 28, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const tool = {
      toolCallId: "escape-hatch",
      invocationId: "escape-hatch",
      turnId: "1",
      name: "bash",
      args: { command: "cargo test" },
      status: "awaiting_approval" as const,
      capabilities: ["execute" as const],
      rationale: "Run focused tests",
      diff: null,
      diffSource: null, chunks: toolOutputBuffer([]),
      display: null, source: null,
      isError: null,
      callIndex: 0,
      timing: { kind: "unknown" as const },
    }
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        permissions: permissionState("strict"),
        tools: { [tool.invocationId]: tool },
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    expect(app.interactionPanel.select.options.map((option) => option.value)).toEqual([
      "allow_once",
      "allow_session",
      "allow_project",
      "review_permission_rule",
      "auto_safe_mode",
      "deny",
    ])
    const always = app.interactionPanel.select.options.findIndex(
      (option) => option.value === "review_permission_rule",
    )
    expect(app.interactionPanel.select.options[always]).toMatchObject({
      name: "Review a permission rule…",
      description: "Review an explicit session pattern",
    })
    app.interactionPanel.select.setSelectedIndex(always)
    app.interactionPanel.select.selectCurrent()
    expect(commands).toEqual([])
    expect(app.picker.title).toContain("Review rule")
    expect(app.picker.select.options.map(item => item.name)).toContain("Enter an explicit allow pattern…")
    expect(app.picker.visible).toBeTrue()
    expect(commands.some(command => command.type === "approve_tool" || command.type === "add_session_permission_rule")).toBeFalse()
    const pattern = app.picker.select.options.findIndex(option => option.value === "add")
    app.picker.select.setSelectedIndex(pattern)
    app.picker.select.selectCurrent()
    await setup.renderOnce()
    expect(app.picker.input.value).toBe("")
    expect(app.picker.title).toContain("this session")
    expect(app.picker.height).toBe(app.composer.y)
    await setup.mockInput.typeText("bash(cargo test*)")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(commands).toContainEqual(expect.objectContaining({ type: "add_session_permission_rule", pattern: "bash(cargo test*)", action: "allow" }))
    expect(commands.some(command => command.type === "approve_tool")).toBeFalse()
    expect(app.state.tools[tool.invocationId]?.status).toBe("awaiting_approval")

    commands.length = 0
    const autoSafe = app.interactionPanel.select.options.findIndex(
      (option) => option.value === "auto_safe_mode",
    )
    app.interactionPanel.select.setSelectedIndex(autoSafe)
    app.interactionPanel.select.selectCurrent()
    await Bun.sleep(0)
    expect(commands).toEqual([
      expect.objectContaining({
        type: "send_message",
        content: "/permissions mode auto-safe",
        attachments: [],
      }),
      expect.objectContaining({
        type: "approve_tool",
        tool_call_id: "escape-hatch",
        invocation_id: "escape-hatch",
        decision: "allow_once",
      }),
    ])

    app.setState({ ...app.state, permissions: permissionState("auto-safe") })
    await setup.renderOnce()
    expect(app.interactionPanel.select.options.map((option) => option.value))
      .not.toContain("auto_safe_mode")
    expect(app.interactionPanel.select.options.map((option) => option.value))
      .toContain("review_permission_rule")

    app.setState({ ...app.state, permissions: null })
    await setup.renderOnce()
    expect(app.interactionPanel.select.options.map((option) => option.value))
      .toContain("auto_safe_mode")
  })

  test("makes unsandboxed bash approvals conspicuous and bounds multiline commands", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    const state: RottweilerState = {
      ...createInitialState(),
      tools: {
        bash: {
          toolCallId: "bash",
          invocationId: "bash",
          turnId: "1",
          name: "bash",
          args: {
            command: "docker build .\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8",
            sandbox: "unsandboxed",
          },
          status: "awaiting_approval",
          capabilities: ["execute", "write_filesystem", "network"],
          rationale: "UNSANDBOXED EXECUTION: this command bypasses native isolation",
          diff: null,
          diffSource: null, chunks: toolOutputBuffer([]),
          display: null, source: null,
          isError: null,
          callIndex: 0,
          timing: { kind: "unknown" },
        },
      },
    }
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: state,
      sessionId: "session-components",
      clientId: "client-components",
      requestId: () => "request-components",
      onCommand() { },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    expect(app.interactionPanel.title).toContain("UNSANDBOXED")
    expect(app.interactionPanel.prompt.plainText).toContain("Run terminal command")
    expect(app.interactionPanel.prompt.plainText).toContain("$ docker build .")
    expect(app.interactionPanel.prompt.plainText).toContain("line 6")
    expect(app.interactionPanel.prompt.plainText).toContain("… 2 more lines")
    expect(app.interactionPanel.prompt.plainText).not.toContain("line 7")
    expect(app.interactionPanel.prompt.plainText).not.toContain("Arguments:")
    expect(app.interactionPanel.prompt.plainText).toContain("UNSANDBOXED EXECUTION")
  })

  test("keeps approval waiting loud and surfaces a rejected approval round trip", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    const state: RottweilerState = {
      ...createInitialState(),
      tools: {
        bash: {
          toolCallId: "bash",
          invocationId: "bash",
          turnId: "1",
          name: "bash",
          args: { command: "cargo test" },
          status: "awaiting_approval",
          capabilities: ["execute"],
          rationale: "Run tests",
          diff: null,
          diffSource: null, chunks: toolOutputBuffer([]),
          display: null, source: null,
          isError: null,
          callIndex: 0,
          timing: { kind: "unknown" },
        },
      },
    }
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: state,
      onCommand(command) {
        if (command.type !== "approve_tool") return { type: "accepted" }
        return {
          type: "rejected",
          error: {
            category: "tool",
            code: "driver_lease_required",
            message: "only the active driver can approve tools",
            retryable: true,
          },
        }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    expect(app.banner.plainText).toContain("Waiting for approval · Terminal command")
    expect(app.statusLine.plainText).toContain("approval · Terminal command")
    expect(app.interactionPanel.prompt.plainText).toContain("Run terminal command")
    expect(app.interactionPanel.prompt.plainText).not.toContain("Arguments:")
    expect(app.interactionPanel.prompt.plainText).not.toContain("execute")

    app.interactionPanel.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.state.errors.at(-1)?.code).toBe("driver_lease_required")
    expect(app.banner.plainText).toContain("only the active driver can approve tools")
  })

  test("renders a completed submitted plan and routes explicit approval", async () => {
    const setup = await createTestRenderer({ width: 112, height: 30, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        mode: "plan",
        pendingPlan: {
          title: "Implement safely",
          summary_md: "One reviewed change.",
          steps: [{ description: "Edit", files_touched: ["src/lib.rs"], verification: "cargo test" }],
          open_questions: ["Keep compatibility?"],
        },
      },
      sessionId: "session-plan",
      clientId: "client-plan",
      requestId: () => "request-plan",
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()
    expect(app.interactionPanel.visible).toBe(true)
    expect(setup.captureCharFrame()).toContain("1. Edit")
    expect(setup.captureCharFrame()).toContain("Files: src/lib.rs")
    expect(setup.captureCharFrame()).toContain("Verify: cargo test")
    setup.mockInput.pressKey("pagedown")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Keep compatibility?")
    app.setState({ ...app.state, pendingPlan: {
      ...app.state.pendingPlan!,
      steps: Array.from({ length: 30 }, (_, index) => ({ description: `Step ${index + 1}`, files_touched: [`src/file${index}.rs`], verification: "cargo test" })),
      open_questions: ["FINAL_PLAN_QUESTION"],
    } })
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("FINAL_PLAN_QUESTION")
    for (let page = 0; page < 20; page++) setup.mockInput.pressKey("\x1b[6~")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("FINAL_PLAN_QUESTION")
    expect(setup.captureCharFrame()).toContain("Approve plan")
    app.interactionPanel.select.selectCurrent()
    expect(commands).toContainEqual({
      type: "approve_plan",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-plan",
        request_id: "request-plan",
      },
      session_id: "session-plan",
      decision: "approve",
      revisions: null,
    })
  })
})
