import { prepareToolDisplay } from "../../src/state/tool-display"
import { conversationItem, toolItem, sessionReaderFor, emptySessionReader } from "../fixtures/history"
import { createStreamingTail } from "../../src/state/model"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { afterEach, describe, expect, test } from "bun:test"
import {
  createTestRenderer,
  MockTreeSitterClient,
  type TestRenderer,
} from "@opentui/core/testing"

import { createRottweilerApp, type RottweilerApp } from "../../src/app"
import type { TranscriptItem } from "../../src/protocol"
import type { RottweilerState, ToolProjection } from "../../src/state"
import { createInitialState } from "../../src/state"

const usage = {
  input_tokens: "1200",
  output_tokens: "380",
  cache_read_tokens: "900",
  cache_write_tokens: "0",
  reasoning_tokens: "40",
}
const money = { kind: "monetary", amount_micros: "12450", currency: "USD" } as const
const TOOLS_FIXTURE_NOW_MS = Date.parse("2026-01-01T12:00:41.000Z")

function fixtureState(): RottweilerState {
  return {
    ...createInitialState(),
    connection: { phase: "connected", attempt: 0, error: null, gap: null },
    mode: "execute",
    model: "fast",
    turns: {
      "1": { turnId: "1", status: "completed", usage, cost: money, timing: { kind: "unknown" } },
    },
    context: {
      turn_id: "1",
      stable_prefix_hash: "stable-fixture",
      used_tokens: "6400",
      usable_tokens: "32000",
      reserved_tokens: "4000",
      context_window_known: true,
      cache_breakpoints: [{ after_item_id: "policy" }, { after_item_id: "tools" }],
      items: [
        {
          item_id: "policy",
          kind: "system",
          label: "System policy",
          source: "engine",
          machine_local_path: null,
          estimated_tokens: "640",
          state: { pinned: true, evicted: false, summarized: false, pruned: false },
        },
        {
          item_id: "instructions",
          kind: "project_instructions",
          label: "AGENTS.md",
          source: "workspace",
          machine_local_path: null,
          estimated_tokens: "220",
          state: { pinned: false, evicted: false, summarized: false, pruned: false },
        },
        {
          item_id: "tool-output",
          kind: "tool_result",
          label: "cargo test output",
          source: "conversation",
          machine_local_path: null,
          estimated_tokens: "1800",
          state: { pinned: false, evicted: false, summarized: false, pruned: true },
        },
      ],
    },
    cost: {
      utc_day: "2026-01-01",
      subscription_quota: null,
      session_usage: usage,
      session_cost_micros_usd: "12450",
      session_ai_credit_micros: "0",
      session_subscription_tokens: "0",
      daily_cost_micros_usd: "12450",
      daily_ai_credit_micros: "0",
      daily_subscription_tokens: "0",
      trailing_minute_cost_micros_usd: "12450",
      trailing_minute_ai_credit_micros: "0",
      trailing_minute_subscription_tokens: "0",
      cache_hit_basis_points: 7500,
      session_cost_cap_micros_usd: "1000000",
      daily_cost_cap_micros_usd: null,
      session_ai_credit_cap_micros: null,
      daily_ai_credit_cap_micros: null,
      session_token_cap: null,
      daily_token_cap: null,
      spend_rate_alarm_micros_usd_per_minute: null,
      ai_credit_rate_alarm_micros_per_minute: null,
      token_rate_alarm_per_minute: null,
      hard_cap_reached: false,
      session_monetary_accounting_complete: true,
      daily_monetary_accounting_complete: true,
      session_subscription_quota_entries: "0",
      session_cost_unavailable_entries: "0",
      session_non_usd_monetary_entries: "0",
      daily_subscription_quota_entries: "0",
      daily_cost_unavailable_entries: "0",
      daily_non_usd_monetary_entries: "0",
    },
    workspaceStatus: {
      workspaceName: "Rottweiler",
      branch: "feature/tui-v1",
      changedPaths: ["packages/tui/src/app.ts"],
      truncated: false,
    },
    commands: [
      { name: "context", description: "Inspect assembled context", usage: "/context" },
      { name: "compact", description: "Compact the conversation", usage: "/compact" },
      { name: "rewind", description: "Restore a prior checkpoint", usage: "/rewind" },
    ],
    models: [
      { id: "openai_codex/fast", displayName: "fast", provider: "openai_codex", aliases: ["fast"], current: false, available: true, status: null, vision: true, thinking: true, toolCalling: true },
      { id: "github_copilot/deep", displayName: "deep", provider: "github_copilot", aliases: ["deep"], current: false, available: true, status: null, vision: false, thinking: true, toolCalling: true },
    ],
    sessions: [
      {
        sessionId: "session-1",
        workspaceName: "Rottweiler",
        model: "fast",
        driverClientId: "client",
        shellActive: false,
      },
      {
        sessionId: "session-2",
        workspaceName: "AeroSpace",
        model: "deep",
        driverClientId: null,
        shellActive: true,
      },
    ],
    workspaceFiles: [
      { path: "packages/tui/src/app.ts", isDirectory: false },
      { path: "packages/tui/src/components", isDirectory: true },
      { path: "PROJECT.md", isDirectory: false },
    ],
  }
}

function pendingTool(diff: boolean): ToolProjection {
  return {
    toolCallId: diff ? "edit-tool" : "bash-tool",
    invocationId: diff ? "edit-tool" : "bash-tool",
    turnId: "2",
    name: diff ? "edit" : "bash",
    args: diff ? { path: "src/main.rs" } : { command: "cargo test" },
    status: "awaiting_approval",
    capabilities: diff ? ["write_filesystem"] : ["execute"],
    rationale: diff ? "Apply the proposed reconnect fix" : "Run the local test suite",
    diff: diff
      ? {
          proposal_id: "proposal",
          path: "src/main.rs",
          unified_diff:
            "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n-old();\n+reconnect();\n",
          arguments_hash: "a",
          base_hash: "b",
          diff_hash: "d",
          truncated: false,
        }
      : null,
    chunks: toolOutputBuffer([]),
    display: null, source: null,
    isError: null,
    callIndex: 0,
    timing: { kind: "unknown" },
  }
}

function historicalItems(): TranscriptItem[] {
  return [
    conversationItem(3, "user", "Replay the saved session without changing it."),
    toolItem(5, "read", '{"path":"PROJECT.md"}', "Historical PROJECT.md contents"),
    conversationItem(8, "assistant", "## Historical result\n\nThe saved event log rendered through the retained TUI."),
    { id: "9", ordinal: "3", revision: "9", agent_turn: "2",
      content: { type: "turn_summary", turn_id: "2", status: "completed", usage, cost: money } },
  ]
}

function conversationItems(): TranscriptItem[] {
  const answer = conversationItem(2, "assistant", "## Done\n\nThe event stream now resumes from the last durable sequence.")
  if (answer.content.type === "conversation") answer.content.blocks.push({
    type: "citation", body: { text: "Protocol contract — https://example.invalid/contract", format: "text", complete: true, source: { sequence: "2", selector: { type: "conversation_block", index: 1 } } },
  })
  return [conversationItem(1, "user", "Add reconnect-safe streaming to the TUI."), answer,
    { id: "3", ordinal: "2", revision: "3", agent_turn: "1",
      content: { type: "turn_summary", turn_id: "1", status: "completed", usage, cost: money } }]
}

function toolsFixtureState(): RottweilerState {
  const startedAtMs = TOOLS_FIXTURE_NOW_MS - 41_000
  const makeTool = (
    toolCallId: string,
    callIndex: number,
    extra: Partial<ToolProjection>,
  ): ToolProjection => ({
    toolCallId,
    invocationId: toolCallId,
    turnId: "tools-turn",
    name: "read",
    args: { path: `${toolCallId}.ts` },
    status: "finished",
    capabilities: [],
    rationale: null,
    diff: null,
    chunks: toolOutputBuffer([]),
    display: prepareToolDisplay({ type: "text", text: "Completed retained output" }, null, { path: `${toolCallId}.ts` }, false), source: null,
    isError: false,
    callIndex,
    timing: { kind: "closed", startedAtMs, finishedAtMs: startedAtMs + 5_000 },
    ...extra,
  })
  const tools = [
    makeTool("read-config", 0, { name: "read", args: { path: "packages/tui/src/app.ts" } }),
    makeTool("search-layout", 1, { name: "grep", args: { pattern: "ToolsWorkspaceRenderable" } }),
    makeTool("run-tests", 2, {
      name: "bash",
      args: { command: "bun test test/components.test.ts" },
      status: "running",
      chunks: toolOutputBuffer([{
        stream: "stdout",
        chunk: Array.from({ length: 12 }, (_, index) => `component check ${index + 1} passed`).join("\n"),
      }]),
      display: null, source: null,
      isError: null,
      timing: { kind: "open", startedAtMs, lastObservedAtMs: startedAtMs + 39_000 },
    }),
    makeTool("denied-edit", 3, {
      name: "edit",
      args: { path: "generated/output.ts" },
      display: prepareToolDisplay({ type: "text", text: "permission denied for tool edit" }, null, { path: "generated/output.ts" }, true), source: null,
      isError: true,
    }),
    makeTool("failed-edit", 4, {
      name: "edit",
      args: { path: "packages/tui/src/app.ts" },
      display: prepareToolDisplay({ type: "text", text: "validation failed" }, null, { path: "packages/tui/src/app.ts" }, true), source: null,
      isError: true,
    }),
    makeTool("explicit-diagnostics", 5, {
      name: "diagnostics",
      args: { path: "packages/tui/src/app.ts" },
      display: prepareToolDisplay({ type: "text", text: "No diagnostics." }, null, { path: "packages/tui/src/app.ts" }, false), source: null,
    }),
  ]
  return {
    ...createInitialState(),
    connection: { phase: "connected", attempt: 0, error: null, gap: null },
    streamingTail: createStreamingTail({
      turnId: "tools-turn",
      text: "",
      thinking: "",
      citations: [],
      toolInvocationIds: tools.map((tool) => tool.toolCallId),
      finished: null,
    }),
    turns: {
      "tools-turn": {
        turnId: "tools-turn",
        status: "running",
        usage: null,
        cost: null,
        timing: { kind: "open", startedAtMs, lastObservedAtMs: startedAtMs + 40_000 },
      },
    },
    tools: Object.fromEntries(tools.map((tool) => [tool.toolCallId, tool])),
    queuedMessages: [
      { position: "1", content: "Run the complete suite" },
      { position: "2", content: "Inspect the direct raster" },
    ],
  }
}

interface ScreenScenario {
  readonly name: string
  readonly state: RottweilerState
  readonly history?: readonly TranscriptItem[]
  readonly setup?: (app: RottweilerApp) => void
  readonly replaySessionId?: string
}

function scenarios(): ScreenScenario[] {
  const base = fixtureState()
  return [
    { name: "01-ready", history: [], state: { ...createInitialState(), connection: base.connection } },
    { name: "02-conversation", state: base },
    {
      name: "03-streaming-thinking-citations",
      state: {
        ...base,
        streamingTail: createStreamingTail({
          turnId: "2",
          text: "I’m updating the retained render tree without touching history…",
          thinking: "Keep the durable cursor separate from command acknowledgements.",
          citations: [{ uri: "https://example.invalid/sse", title: "SSE contract" }],
          toolInvocationIds: [],
          finished: null,
        }),
      },
    },
    {
      name: "04-live-tool-output",
      state: {
        ...base,
        streamingTail: createStreamingTail({
          turnId: "2",
          text: "Running focused checks.",
          thinking: "",
          citations: [],
          toolInvocationIds: ["live-tool"],
          finished: null,
        }),
        tools: {
          "live-tool": {
            ...pendingTool(false),
            toolCallId: "live-tool",
            invocationId: "live-tool",
            status: "running",
            chunks: toolOutputBuffer([{ stream: "stdout", chunk: "test transport ... ok\ntest reducer ..." }]),
          },
        },
      },
    },
    { name: "05-diff-approval", state: { ...base, tools: { edit: pendingTool(true) } } },
    { name: "06-generic-permission", state: { ...base, tools: { bash: pendingTool(false) } } },
    {
      name: "07-ask-user",
      state: {
        ...base,
        questions: {
          question: {
            questionId: "question",
            turnId: "2",
            questions: [
              {
                id: "question",
                prompt: "Which validation scope should run next?",
                response_kind: "select_one",
                options: [
                  { value: "focused", label: "Focused", description: "Fast local checks" },
                  { value: "full", label: "Full suite", description: "All workspace tests" },
                ],
              },
            ],
          },
        },
      },
    },
    { name: "08-context-surgery", state: base },
    { name: "09-command-picker", state: base, setup: (app) => app.openCommandPicker() },
    { name: "10-file-picker", state: base, setup: (app) => app.openFilePicker("") },
    { name: "11-model-picker", state: base, setup: (app) => app.openModelPicker() },
    { name: "12-session-picker", state: base, setup: (app) => app.openSessionPicker() },
    {
      name: "13-reconnect-replay",
      state: {
        ...base,
        connection: {
          phase: "replaying",
          attempt: 2,
          error: "stream closed",
          gap: { expected: "41", received: "46" },
        },
      },
    },
    {
      name: "14-compaction",
      state: {
        ...base,
        compaction: {
          active: true,
          reason: "automatic",
          summaryTurnId: null,
          reclaimedTokens: null,
          attempt: null,
          text: "",
          thinking: "",
        },
      },
    },
    {
      name: "15-budget-cap",
      state: {
        ...base,
        budgets: [
          {
            turnId: "2",
            level: "hard_cap",
            scope: "session",
            unit: "micros_usd",
            current: "1000000",
            limit: "1000000",
          },
        ],
      },
    },
    {
      name: "16-subagent-orchestration",
      state: {
        ...base,
        streamingTail: createStreamingTail({
          turnId: "2",
          text: "I’m collating three isolated reviews in deterministic order.",
          thinking: "",
          citations: [],
          toolInvocationIds: [],
          finished: null,
        }),
        subagentOrder: ["explore", "tests", "review"],
        subagents: {
          explore: {
            projectionId: "explore",
            subagentId: "explore",
            parentTurnId: "2",
            task: "Map orchestration boundaries",
            spawnedAtMs: null,
            status: "completed",
            childSessionId: "child-explore",
            lastChildSequence: "12",
            activity: "finished",
            summary: "Found 8 integration points",
            touchedFileCount: 0,
            diffArtifactId: null,
          },
          tests: {
            projectionId: "tests",
            subagentId: "tests",
            parentTurnId: "2",
            task: "Exercise worktree isolation",
            spawnedAtMs: null,
            status: "running",
            childSessionId: "child-tests",
            lastChildSequence: "7",
            activity: "using tool · cargo test",
            summary: null,
            touchedFileCount: 0,
            diffArtifactId: null,
          },
          review: {
            projectionId: "review",
            subagentId: "review",
            parentTurnId: "2",
            task: "Adversarially review permissions",
            spawnedAtMs: null,
            status: "running",
            childSessionId: "child-review",
            lastChildSequence: "3",
            activity: "thinking",
            summary: null,
            touchedFileCount: 0,
            diffArtifactId: null,
          },
        },
      },
    },
    {
      name: "17-historical-session-replay",
      state: { ...createInitialState(), connection: base.connection },
      history: historicalItems(),
      replaySessionId: "session-golden-replay",
      setup: (app) => {
        app.handleEvent({
          type: "session_history_ready",
          meta: {
            protocol_version: 1,
            client_id: "golden-client",
            request_id: "golden-request",
            emitted_at: "2026-01-01T00:00:00Z",
          },
          session_id: "session-golden-replay",
          through_sequence: "9",
        })
      },
    },
    {
      name: "18-cumulative-session-review",
      state: {
        ...base,
        review: {
          sessionId: "session-review-golden",
          files: [
            {
              path: "packages/tui/src/app.ts",
              unifiedDiff:
                "--- a/packages/tui/src/app.ts\n+++ b/packages/tui/src/app.ts\n@@ -1 +1 @@\n-live();\n+replay();\n",
              status: "pending",
              truncated: false,
              unrestorableReason: null,
              originalHash: "old",
              currentHash: "new",
            },
            {
              path: "generated/output.bin",
              unifiedDiff: "Binary files differ",
              status: "pending",
              truncated: false,
              unrestorableReason: "original bytes were not checkpointed",
              originalHash: "absent",
              currentHash: "generated",
            },
          ],
        },
      },
      setup: (app) => app.openReview(),
    },
    { name: "19-theme-browser", state: base, setup: (app) => app.openThemePicker() },
  ]
}

describe("M4 golden screens", () => {
  let renderer: TestRenderer | undefined
  let treeSitter: MockTreeSitterClient | undefined

  afterEach(async () => {
    renderer?.destroy()
    renderer = undefined
    await treeSitter?.destroy()
    treeSitter = undefined
  })

  for (const scenario of scenarios()) {
    test(scenario.name, async () => {
      const setup = await createTestRenderer({ width: 112, height: 32, useThread: false })
      renderer = setup.renderer
      treeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
      treeSitter.setMockResult({ highlights: [] })
      const items = scenario.history ?? conversationItems()
      let reads = 0
      const source = sessionReaderFor(items)
      const app = createRottweilerApp(renderer, { sessionReader: { ...source, page: async (...args) => {
        const result = await source.page(...args)
        reads += 1
        return result
      } },
        initialState: scenario.state,
        requestId: () => "golden-request",
        treeSitterClient: treeSitter,
        ...(scenario.replaySessionId === undefined
          ? {}
          : { replaySessionId: scenario.replaySessionId }),
      })
      renderer.root.add(app)
      scenario.setup?.(app)
      await setup.waitFor(() => reads > 0)
      await setup.renderOnce()
      await setup.waitFor(() => treeSitter?.isHighlighting() === false)
      await setup.flush()
      const frame = setup
        .captureCharFrame()
        .split("\n")
        .map((line) => line.trimEnd())
        .join("\n")
      const spans = setup.captureSpans()
      const styled = spans.lines.map((line) =>
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
    })
  }

  test("Tools workspace keeps exact production cells at 110 by 32", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader,
      initialState: toolsFixtureState(),
      nowMs: () => TOOLS_FIXTURE_NOW_MS,
    })
    renderer.root.add(app)
    app.showToolsView()
    await setup.flush()
    const lines = setup.captureCharFrame().split("\n").slice(0, 32)

    expect(lines).toHaveLength(32)
    expect(app.main.height).toBe(27)
    expect(lines.every((line) => Bun.stringWidth(line) === 110)).toBeTrue()
    expect(lines.slice(0, 27).map((line) => line[74])).toEqual(Array(27).fill("│"))
    expect(lines[0]?.slice(1)).toStartWith("● rottweiler  running tools")
    expect(lines[0]?.slice(75)).toStartWith("THIS TURN")
    expect(lines.join("\n")).toContain("bun test test/components.test.ts")
    expect(lines.join("\n")).toContain("validation failed")
    expect(lines.join("\n")).toContain("denied")
    expect(lines.join("\n")).not.toContain("D I A G N O S T I C S")
    expect(lines.join("\n")).not.toContain("BACKGROUND")
  })

  test("Tools workspace removes its rail below 100 columns", async () => {
    const setup = await createTestRenderer({ width: 99, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader,
      initialState: toolsFixtureState(),
      nowMs: () => TOOLS_FIXTURE_NOW_MS,
    })
    renderer.root.add(app)
    app.showToolsView()
    await setup.flush()
    const lines = setup.captureCharFrame().split("\n").slice(0, 32)

    expect(lines.every((line) => Bun.stringWidth(line) === 99)).toBeTrue()
    expect(lines[0]).toContain("● rottweiler  running tools")
    expect(lines.slice(0, 27).join("\n")).not.toContain("THIS TURN")
  })

  test("Tools workspace keeps a usable scroller on a short terminal", async () => {
    const setup = await createTestRenderer({ width: 110, height: 11, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader,
      initialState: toolsFixtureState(),
      nowMs: () => TOOLS_FIXTURE_NOW_MS,
    })
    renderer.root.add(app)
    app.showToolsView()
    await setup.flush()
    const frame = setup.captureCharFrame()
    const lines = frame.split("\n").slice(0, 11)

    expect(lines.every((line) => Bun.stringWidth(line) === 110)).toBeTrue()
    expect(frame).toContain("● rottweiler  running tools")
    expect(frame).not.toContain("THIS TURN")
    expect(app.toolsWorkspace.activityScroller.height).toBeGreaterThanOrEqual(1)
  })
})

function stableDigest(value: string): string {
  let hash = 2_166_136_261
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index)
    hash = Math.imul(hash, 16_777_619)
  }
  return (hash >>> 0).toString(16).padStart(8, "0")
}
