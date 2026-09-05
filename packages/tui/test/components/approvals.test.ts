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
import { emptyHistoryReader } from "../fixtures/history"
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
          chunks: toolOutputBuffer([]),
          output: null,
          isError: null,
          callIndex: 0,
          timing: { kind: "unknown" },
        },
      },
    }
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
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
      chunks: toolOutputBuffer([]),
      output: null,
      isError: null,
      callIndex: 0,
      timing: { kind: "unknown" as const },
    }
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: { ...createInitialState(), tools: { [tool.invocationId]: tool } },
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
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
      chunks: toolOutputBuffer([]),
      output: null,
      isError: null,
      callIndex: 0,
      timing: { kind: "unknown" as const },
    }
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
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
      "allow_tool_session",
      "auto_safe_mode",
      "deny",
    ])
    const always = app.interactionPanel.select.options.findIndex(
      (option) => option.value === "allow_tool_session",
    )
    expect(app.interactionPanel.select.options[always]).toMatchObject({
      name: "Always allow Terminal command",
      description: "This session · any arguments",
    })
    app.interactionPanel.select.setSelectedIndex(always)
    app.interactionPanel.select.selectCurrent()
    expect(commands).toEqual([
      expect.objectContaining({
        type: "add_session_permission_rule",
        pattern: "bash(*)",
        action: "allow",
      }),
      expect.objectContaining({
        type: "approve_tool",
        tool_call_id: "escape-hatch",
        invocation_id: "escape-hatch",
        decision: "allow_once",
      }),
    ])

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
      .toContain("allow_tool_session")

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
          chunks: toolOutputBuffer([]),
          output: null,
          isError: null,
          callIndex: 0,
          timing: { kind: "unknown" },
        },
      },
    }
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
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
          chunks: toolOutputBuffer([]),
          output: null,
          isError: null,
          callIndex: 0,
          timing: { kind: "unknown" },
        },
      },
    }
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
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
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        mode: "plan",
        pendingPlan: {
          title: "Implement safely",
          summary_md: "One reviewed change.",
          steps: [{ description: "Edit", files_touched: ["src/lib.rs"], verification: "cargo test" }],
          open_questions: [],
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
