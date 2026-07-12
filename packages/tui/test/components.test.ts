import { afterEach, describe, expect, test } from "bun:test"
import { CliRenderEvents } from "@opentui/core"
import {
  createTestRenderer,
  MockTreeSitterClient,
  setRendererCapabilities,
  type TestRenderer,
} from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import { ContextPanelRenderable, ImageAttachmentRenderable, fuzzyScore } from "../src/components"
import {
  PROTOCOL_VERSION,
  type ClientCommand,
  type CommandOutcome,
  type EngineEvent,
} from "../src/protocol"
import { createInitialState, type RottweilerState } from "../src/state"
import { kennelTheme } from "../src/theme"

function meta(sequence: string) {
  return {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-components",
    sequence_id: sequence,
    emitted_at: "2026-01-01T00:00:00Z",
  }
}

async function waitFor(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const deadline = performance.now() + timeoutMs
  while (!predicate()) {
    if (performance.now() >= deadline) throw new Error("timed out waiting for component state")
    await Bun.sleep(5)
  }
}

describe("M4 retained components", () => {
  let renderer: TestRenderer | undefined
  let treeSitter: MockTreeSitterClient | undefined

  afterEach(async () => {
    renderer?.destroy()
    renderer = undefined
    await treeSitter?.destroy()
    treeSitter = undefined
  })

  test("mounts only visible transcript rows and preserves the streaming markdown instance", async () => {
    const setup = await createTestRenderer({ width: 86, height: 24, useThread: false })
    renderer = setup.renderer
    treeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
    treeSitter.setMockResult({ highlights: [] })
    const transcript = Array.from({ length: 10_000 }, (_, index) => ({
      sequenceId: String(index + 1),
      agentTurn: String(index + 1),
      turn: {
        role: "assistant" as const,
        blocks: [{ type: "text" as const, text: `Turn ${index} stayed virtualized.` }],
        meta: { synthetic: false, summary: false },
      },
    }))
    const initial: RottweilerState = {
      ...createInitialState(),
      transcript,
      streamingTail: {
        turnId: "10001",
        text: "first",
        thinking: "",
        citations: [],
        toolCallIds: [],
        finished: null,
      },
    }
    const app = createRottweilerApp(renderer, {
      initialState: initial,
      treeSitterClient: treeSitter,
    })
    renderer.root.add(app)
    await setup.waitFor(() => treeSitter?.isHighlighting() === false)
    await setup.flush()

    expect(app.transcript.mountedEntryCount).toBeLessThan(20)
    const streamingMarkdown = app.transcript.streamingMarkdown
    app.setState({
      ...initial,
      streamingTail: { ...initial.streamingTail!, text: "first second" },
    })
    await setup.renderOnce()
    expect(app.transcript.streamingMarkdown).toBe(streamingMarkdown)
    expect(app.transcript.mountedEntryCount).toBeLessThan(20)

    app.transcript.setScrollOffset(5_000_000)
    await setup.flush()
    expect(app.transcript.mountedEntryCount).toBeLessThan(20)
    expect(app.transcript.mountedKeys.at(-1)).not.toContain(":0:")
  })

  test("routes diff approval through generated commands", async () => {
    const setup = await createTestRenderer({ width: 112, height: 30, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const state: RottweilerState = {
      ...createInitialState(),
      tools: {
        edit: {
          toolCallId: "edit",
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
          chunks: [],
          output: null,
          isError: null,
          callIndex: 0,
        },
      },
    }
    const app = createRottweilerApp(renderer, {
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
        decision: "deny",
      }),
    )
  })

  test("makes unsandboxed bash approvals conspicuous with the exact command", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    const state: RottweilerState = {
      ...createInitialState(),
      tools: {
        bash: {
          toolCallId: "bash",
          turnId: "1",
          name: "bash",
          args: { command: "docker build .", sandbox: "unsandboxed" },
          status: "awaiting_approval",
          capabilities: ["execute", "write_filesystem", "network"],
          rationale: "UNSANDBOXED EXECUTION: this command bypasses native isolation",
          diff: null,
          chunks: [],
          output: null,
          isError: null,
          callIndex: 0,
        },
      },
    }
    const app = createRottweilerApp(renderer, {
      initialState: state,
      sessionId: "session-components",
      clientId: "client-components",
      requestId: () => "request-components",
      onCommand() {},
    })
    renderer.root.add(app)
    await setup.renderOnce()

    expect(app.interactionPanel.title).toContain("UNSANDBOXED")
    expect(app.interactionPanel.prompt.plainText).toContain("$ docker build .")
    expect(app.interactionPanel.prompt.plainText).toContain("UNSANDBOXED EXECUTION")
  })

  test("renders a completed submitted plan and routes explicit approval", async () => {
    const setup = await createTestRenderer({ width: 112, height: 30, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
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

  test("notifies only while terminal focus is away", async () => {
    const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
    renderer = setup.renderer
    const notifications: string[] = []
    const app = createRottweilerApp(renderer, {
      notifications: {
        notify(notification) {
          notifications.push(notification.kind)
        },
      },
    })
    renderer.root.add(app)
    renderer.emit(CliRenderEvents.BLUR)
    const events: EngineEvent[] = [
      { type: "turn_started", meta: meta("1"), turn_id: "1" },
      {
        type: "turn_finished",
        meta: meta("2"),
        turn_id: "1",
        status: "completed",
        usage: {
          input_tokens: "1",
          output_tokens: "1",
          cache_read_tokens: "0",
          cache_write_tokens: "0",
          reasoning_tokens: "0",
        },
        cost: { kind: "monetary", amount_micros: "1", currency: "USD" },
      },
    ]
    for (const event of events) {
      app.handleEvent(event)
    }
    app.handleEvent({
      type: "ui_notification",
      meta: meta("3"),
      plugin_id: "reviewer",
      title: "Review ready",
      message: "Open the result",
    })
    renderer.emit(CliRenderEvents.FOCUS)
    app.handleEvent({ type: "turn_started", meta: meta("4"), turn_id: "2" })
    app.handleEvent({
      type: "turn_finished",
      meta: meta("5"),
      turn_id: "2",
      status: "completed",
      usage: events[1]!.type === "turn_finished" ? events[1]!.usage : neverUsage(),
      cost:
        events[1]!.type === "turn_finished"
          ? events[1]!.cost
          : { kind: "unavailable", reason: "fixture" },
    })

    expect(notifications).toEqual(["turn_finished", "plugin"])
  })

  test("renders compact nested subagent progress without replacing retained rows", async () => {
    const setup = await createTestRenderer({ width: 92, height: 20, useThread: false })
    renderer = setup.renderer
    const initial: RottweilerState = {
      ...createInitialState(),
      streamingTail: {
        turnId: "1",
        text: "Coordinating the implementation.",
        thinking: "",
        citations: [],
        toolCallIds: [],
        finished: null,
      },
      subagentOrder: ["explore", "tests"],
      subagents: {
        explore: {
          projectionId: "explore",
          subagentId: "explore",
          parentTurnId: "1",
          task: "Inspect provider boundaries",
          status: "running",
          childSessionId: "session-explore",
          lastChildSequence: "4",
          activity: "using tool · read",
          summary: null,
          touchedFileCount: 0,
          diffArtifactId: null,
        },
        tests: {
          projectionId: "tests",
          subagentId: "tests",
          parentTurnId: "1",
          task: "Add orchestration tests",
          status: "completed",
          childSessionId: "session-tests",
          lastChildSequence: "8",
          activity: "finished",
          summary: "Added deterministic coverage",
          touchedFileCount: 2,
          diffArtifactId: "diff-tests",
        },
      },
    }
    const app = createRottweilerApp(renderer, { initialState: initial })
    renderer.root.add(app)
    await setup.renderOnce()

    const frame = setup.captureCharFrame()
    expect(frame).toContain("Subagents · 1 running · 2 total")
    expect(frame).toContain("Inspect provider boundaries · using tool · read")
    expect(frame).toContain("Add orchestration tests · Added deterministic coverage")
    expect(app.transcript.subagentPanel.rows.size).toBe(2)

    app.setState({
      ...initial,
      streamingTail: null,
      transcript: [
        {
          sequenceId: "9",
          agentTurn: "1",
          turn: {
            role: "assistant",
            blocks: [{ type: "text", text: "Delegated checks are complete." }],
            meta: { synthetic: false, summary: false },
          },
        },
      ],
      subagentOrder: ["tests"],
      subagents: { tests: initial.subagents.tests! },
    })
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Added deterministic coverage · 2 files · diff ready")

    const many = Object.fromEntries(
      Array.from({ length: 20 }, (_, index) => [
        `child-${index}`,
        {
          projectionId: `child-${index}`,
          subagentId: `child-${index}`,
          parentTurnId: "2",
          task: `Bounded child ${index}`,
          status: index < 4 ? ("running" as const) : ("completed" as const),
          childSessionId: `session-${index}`,
          lastChildSequence: String(index),
          activity: index < 4 ? "working" : "finished",
          summary: index < 4 ? null : `result ${index}`,
          touchedFileCount: 0,
          diffArtifactId: null,
        },
      ]),
    )
    app.setState({
      ...initial,
      streamingTail: { ...initial.streamingTail!, turnId: "2" },
      subagentOrder: Object.keys(many),
      subagents: many,
    })
    await setup.renderOnce()
    expect(app.transcript.subagentPanel.rows.size).toBe(8)
    expect(app.transcript.subagentPanel.header.plainText).toContain("20 total")
  })

  test("renders cumulative review and routes exact per-file accept or revert commands", async () => {
    const setup = await createTestRenderer({ width: 112, height: 32, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const review = {
      sessionId: "session-review",
      files: [
        {
          path: "src/lib.rs",
          unifiedDiff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
          status: "pending" as const,
          truncated: true,
          unrestorableReason: null,
          originalHash: "old",
          currentHash: "new",
        },
        {
          path: "generated.bin",
          unifiedDiff: "Binary files differ",
          status: "pending" as const,
          truncated: false,
          unrestorableReason: "original bytes were not checkpointed",
          originalHash: "absent",
          currentHash: "generated",
        },
      ],
    }
    const app = createRottweilerApp(renderer, {
      initialState: { ...createInitialState(), review },
      sessionId: "session-review",
      clientId: "review-client",
      requestId: () => "review-request",
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
    app.openReview()
    await setup.renderOnce()

    expect(app.reviewPanel.visible).toBeTrue()
    expect(app.reviewPanel.summary.plainText).toContain("2 pending")
    expect(app.reviewPanel.diff.diff).toContain("+new")
    setup.mockInput.pressKey("r")
    expect(commands).toContainEqual({
      type: "review_file",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "review-client",
        request_id: "review-request",
      },
      session_id: "session-review",
      path: "src/lib.rs",
      decision: "revert",
      current_hash: "new",
    })

    commands.length = 0
    app.reviewPanel.files.setSelectedIndex(1)
    setup.mockInput.pressKey("r")
    expect(commands).toEqual([])
    expect(app.reviewPanel.hint.plainText).toContain("revert unavailable")
    setup.mockInput.pressKey("a")
    expect(commands).toContainEqual(expect.objectContaining({
      type: "review_file",
      path: "generated.bin",
      decision: "accept",
      current_hash: "generated",
    }))
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.reviewPanel.visible).toBeFalse()
    expect(app.composer.visible).toBeTrue()
    expect(app.state.review).toEqual(review)
  })

  test("keeps one review decision in flight and surfaces a stale fingerprint rejection", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    let resolveDecision!: (outcome: CommandOutcome) => void
    const decision = new Promise<CommandOutcome>((resolve) => {
      resolveDecision = resolve
    })
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        review: {
          sessionId: "session-stale-review",
          files: [
            {
              path: "src/stale.rs",
              unifiedDiff: "--- a/src/stale.rs\n+++ b/src/stale.rs\n@@ -1 +1 @@\n-old\n+new\n",
              status: "pending",
              truncated: false,
              unrestorableReason: null,
              originalHash: "original-state",
              currentHash: "displayed-state",
            },
          ],
        },
      },
      sessionId: "session-stale-review",
      onCommand(command) {
        commands.push(command)
        return command.type === "review_file" ? decision : { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openReview()
    await setup.renderOnce()

    setup.mockInput.pressKey("r")
    setup.mockInput.pressKey("r")
    expect(commands.filter((command) => command.type === "review_file")).toHaveLength(1)
    expect(app.reviewPanel.hint.plainText).toContain("Decision pending")

    resolveDecision({
      type: "rejected",
      error: {
        category: "protocol",
        code: "stale_review_fingerprint",
        message: "the file changed since this review was displayed",
        retryable: true,
      },
    })
    await waitFor(() => app.state.errors.at(-1)?.code === "stale_review_fingerprint")
    expect(app.state.errors.at(-1)?.code).toBe("stale_review_fingerprint")
    expect(app.banner.plainText).toContain("file changed since this review")
    expect(app.reviewPanel.hint.plainText).not.toContain("pending")
  })

  test("shows only todos and changed files in the sidebar and opens exact paths", async () => {
    const setup = await createTestRenderer({ width: 52, height: 24, useThread: false })
    renderer = setup.renderer
    const opened: string[] = []
    const panel = new ContextPanelRenderable(renderer, kennelTheme, {
      onOpenDiff: (path) => opened.push(path),
    })
    renderer.root.add(panel)
    panel.update({
      ...createInitialState(),
      todos: [
        { id: "audit", content: "Audit interactions", status: "in_progress" },
        { id: "tests", content: "Add regression tests", status: "pending" },
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
    const frame = setup.captureCharFrame()
    expect(frame).toContain("Todos")
    expect(frame).toContain("Changed files")
    expect(frame).not.toContain("context")

    panel.changedFiles.focus()
    panel.changedFiles.setSelectedIndex(0)
    setup.mockInput.pressEnter()
    expect(opened).toEqual(["src/shared.rs"])

    await setup.mockMouse.click(panel.changedFiles.x + 2, panel.changedFiles.y + 1)
    expect(opened).toEqual(["src/shared.rs", "src/from-status.rs"])
  })

  test("opens the exact retained diff from the default changed-files sidebar", async () => {
    const setup = await createTestRenderer({ width: 112, height: 30, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
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
        unified_diff: "--- a/src/exact.rs\n+++ b/src/exact.rs\n-old\n+exact\n",
        truncated: false,
        binary: false,
      },
    })
    expect(app.reviewPanel.diff.diff).toContain("+exact")
    expect(app.composer.visible).toBeFalse()

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

function neverUsage() {
  return {
    input_tokens: "0",
    output_tokens: "0",
    cache_read_tokens: "0",
    cache_write_tokens: "0",
    reasoning_tokens: "0",
  }
}
