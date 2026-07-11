import { afterEach, describe, expect, test } from "bun:test"
import {
  createTestRenderer,
  MockTreeSitterClient,
  type TestRenderer,
} from "@opentui/core/testing"

import { createRottweilerApp, type RottweilerApp } from "../../src/app"
import type { EngineEvent } from "../../src/protocol"
import type { RottweilerState, ToolProjection } from "../../src/state"
import { createInitialState, engineEvent, reduceRottweilerState } from "../../src/state"

const usage = {
  input_tokens: "1200",
  output_tokens: "380",
  cache_read_tokens: "900",
  cache_write_tokens: "0",
  reasoning_tokens: "40",
}
const money = { kind: "monetary", amount_micros: "12450", currency: "USD" } as const

function fixtureState(): RottweilerState {
  return {
    ...createInitialState(),
    connection: { phase: "connected", attempt: 0, error: null, gap: null },
    mode: "execute",
    model: "fast",
    transcript: [
      {
        sequenceId: "1",
        agentTurn: "1",
        turn: {
          role: "user",
          blocks: [{ type: "text", text: "Add reconnect-safe streaming to the TUI." }],
          meta: { synthetic: false, summary: false },
        },
      },
      {
        sequenceId: "2",
        agentTurn: "1",
        turn: {
          role: "assistant",
          blocks: [
            {
              type: "text",
              text: "## Done\n\nThe event stream now resumes from the last durable sequence.",
            },
            {
              type: "citation",
              uri: "https://example.invalid/contract",
              title: "Protocol contract",
            },
          ],
          meta: { synthetic: false, summary: false, model: "fixture-fast" },
        },
      },
    ],
    turns: {
      "1": { turnId: "1", status: "completed", usage, cost: money },
    },
    context: {
      turn_id: "1",
      stable_prefix_hash: "stable-fixture",
      used_tokens: "6400",
      usable_tokens: "32000",
      reserved_tokens: "4000",
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
      turns: [],
      session_usage: usage,
      session_cost_micros_usd: "12450",
      session_ai_credit_micros: "0",
      daily_cost_micros_usd: "12450",
      daily_ai_credit_micros: "0",
      trailing_minute_cost_micros_usd: "12450",
      trailing_minute_ai_credit_micros: "0",
      cache_hit_basis_points: 7500,
      session_cost_cap_micros_usd: "1000000",
      daily_cost_cap_micros_usd: null,
      session_ai_credit_cap_micros: null,
      daily_ai_credit_cap_micros: null,
      spend_rate_alarm_micros_usd_per_minute: null,
      ai_credit_rate_alarm_micros_per_minute: null,
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
      { alias: "fast", vision: true, thinking: true, toolCalling: true },
      { alias: "deep", vision: false, thinking: true, toolCalling: true },
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
    chunks: [],
    output: null,
    isError: null,
    callIndex: 0,
  }
}

function replayFixtureState(): RottweilerState {
  const eventMeta = (sequence: string) => ({
    protocol_version: 1,
    session_id: "session-golden-replay",
    sequence_id: sequence,
    emitted_at: `2026-01-01T00:00:0${sequence}Z`,
  })
  const events: EngineEvent[] = [
    { type: "mode_changed", meta: eventMeta("1"), mode: "execute" },
    { type: "model_changed", meta: eventMeta("2"), model: "fast" },
    {
      type: "conversation_turn_committed",
      meta: eventMeta("3"),
      agent_turn: "1",
      turn: {
        role: "user",
        blocks: [{ type: "text", text: "Replay the saved session without changing it." }],
        meta: { synthetic: false, summary: false },
      },
    },
    { type: "turn_started", meta: eventMeta("4"), turn_id: "2" },
    {
      type: "tool_call_started",
      meta: eventMeta("5"),
      turn_id: "2",
      tool_call_id: "historical-read",
      name: "read",
      args: { path: "PROJECT.md" },
      call_index: 0,
    },
    {
      type: "tool_call_finished",
      meta: eventMeta("6"),
      turn_id: "2",
      tool_call_id: "historical-read",
      output: { type: "text", text: "Historical PROJECT.md contents" },
      is_error: false,
      call_index: 0,
    },
    {
      type: "conversation_turn_committed",
      meta: eventMeta("7"),
      agent_turn: "2",
      turn: {
        role: "assistant",
        blocks: [
          {
            type: "text",
            text: "## Historical result\n\nThe saved event log rendered through the retained TUI.",
          },
        ],
        meta: { synthetic: false, summary: false, model: "fixture-fast" },
      },
    },
    {
      type: "turn_finished",
      meta: eventMeta("8"),
      turn_id: "2",
      status: "completed",
      usage,
      cost: money,
    },
  ]
  const replayed = events.reduce(
    (state, event) => reduceRottweilerState(state, engineEvent(event)),
    createInitialState(),
  )
  return {
    ...replayed,
    connection: { phase: "connected", attempt: 0, error: null, gap: null },
  }
}

interface ScreenScenario {
  readonly name: string
  readonly state: RottweilerState
  readonly setup?: (app: RottweilerApp) => void
  readonly replaySessionId?: string
}

function scenarios(): ScreenScenario[] {
  const base = fixtureState()
  return [
    { name: "01-ready", state: { ...createInitialState(), connection: base.connection } },
    { name: "02-conversation", state: base },
    {
      name: "03-streaming-thinking-citations",
      state: {
        ...base,
        streamingTail: {
          turnId: "2",
          text: "I’m updating the retained render tree without touching history…",
          thinking: "Keep the durable cursor separate from command acknowledgements.",
          citations: [{ uri: "https://example.invalid/sse", title: "SSE contract" }],
          toolCallIds: [],
          finished: null,
        },
      },
    },
    {
      name: "04-live-tool-output",
      state: {
        ...base,
        streamingTail: {
          turnId: "2",
          text: "Running focused checks.",
          thinking: "",
          citations: [],
          toolCallIds: ["live-tool"],
          finished: null,
        },
        tools: {
          "live-tool": {
            ...pendingTool(false),
            toolCallId: "live-tool",
            status: "running",
            chunks: [{ stream: "stdout", chunk: "test transport ... ok\ntest reducer ..." }],
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
            answered: false,
            answers: null,
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
        compaction: { active: true, reason: "automatic", summaryTurnId: null, reclaimedTokens: null },
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
        streamingTail: {
          turnId: "2",
          text: "I’m collating three isolated reviews in deterministic order.",
          thinking: "",
          citations: [],
          toolCallIds: [],
          finished: null,
        },
        subagentOrder: ["explore", "tests", "review"],
        subagents: {
          explore: {
            projectionId: "explore",
            subagentId: "explore",
            parentTurnId: "2",
            task: "Map orchestration boundaries",
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
      state: replayFixtureState(),
      replaySessionId: "session-golden-replay",
      setup: (app) => {
        app.handleEvent({
          type: "session_replay_completed",
          meta: {
            protocol_version: 1,
            client_id: "golden-client",
            request_id: "golden-request",
            emitted_at: "2026-01-01T00:00:00Z",
          },
          session_id: "session-golden-replay",
          through_sequence: "8",
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
    },
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
      const app = createRottweilerApp(renderer, {
        initialState: scenario.state,
        requestId: () => "golden-request",
        treeSitterClient: treeSitter,
        ...(scenario.replaySessionId === undefined
          ? {}
          : { replaySessionId: scenario.replaySessionId }),
      })
      renderer.root.add(app)
      scenario.setup?.(app)
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
})

function stableDigest(value: string): string {
  let hash = 2_166_136_261
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index)
    hash = Math.imul(hash, 16_777_619)
  }
  return (hash >>> 0).toString(16).padStart(8, "0")
}
