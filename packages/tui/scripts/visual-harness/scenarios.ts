import { prepareToolDisplay } from "../../src/state/tool-display"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { createStreamingTail } from "../../src/state/model"
import type { RottweilerState, ToolProjection } from "../../src/state"
import { createInitialState, emptyTodos } from "../../src/state"

export type VisualScenario = "conversation" | "command-palette" | "approval" | "tools" | "theme-browser" | "settings-browser" | "mcp-browser" | "session-review"
export const TOOLS_FIXTURE_NOW_MS = Date.parse("2026-01-01T12:00:41.000Z")

export function scenarioState(scenario: VisualScenario): RottweilerState {
  if (scenario === "tools") return toolsState()
  if (scenario === "settings-browser") return settingsState()
  if (scenario === "mcp-browser") return mcpState()
  if (scenario === "session-review") return sessionReviewState()
  const state = conversationState()
  if (scenario !== "approval") return state
  const approval = tool({
    toolCallId: "approval-tool",
    name: "bash",
    args: { command: "cargo test -p rw-core" },
    status: "awaiting_approval",
    capabilities: ["execute"],
    rationale: "Run the focused reconnect regression suite",
    display: null, source: null,
    chunks: toolOutputBuffer([]),
    isError: null,
  })
  return {
    ...state,
    streamingTail: createStreamingTail({
      turnId: "2",
      text: "I need permission before running the focused regression suite.",
      thinking: "The command executes workspace code, so it must cross the approval boundary.",
      citations: [],
      toolInvocationIds: [approval.toolCallId],
      finished: null,
    }),
    tools: { [approval.toolCallId]: approval },
  }
}

function sessionReviewState(): RottweilerState {
  return {
    ...conversationState(),
    review: {
      sessionId: "visual-session",
      files: [
        {
          path: "src/cursor.rs",
          unifiedDiff: "--- a/src/cursor.rs\n+++ b/src/cursor.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+added\n context\n",
          status: "pending",
          truncated: false,
          unrestorableReason: null,
          originalHash: "cursor-before",
          currentHash: "cursor-after",
        },
        {
          path: "packages/tui/src/app.ts",
          unifiedDiff: "--- a/packages/tui/src/app.ts\n+++ b/packages/tui/src/app.ts\n@@ -1 +1 @@\n-before\n+after\n",
          status: "accepted",
          truncated: false,
          unrestorableReason: null,
          originalHash: "app-before",
          currentHash: "app-after",
        },
        {
          path: "generated/report.txt",
          unifiedDiff: "--- /dev/null\n+++ b/generated/report.txt\n@@ -0,0 +1 @@\n+generated\n",
          status: "pending",
          truncated: false,
          unrestorableReason: "original bytes were not checkpointed",
          originalHash: "absent",
          currentHash: "report-after",
        },
      ],
    },
  }
}

function mcpState(): RottweilerState {
  const base = conversationState()
  return {
    ...base,
    commands: [
      ...base.commands,
      { name: "mcp", description: "Inspect and manage MCP connections", usage: "/mcp" },
    ],
    mcpServers: [
      { name: "docs.remote", enabled: true, approved: true, state: { type: "ready" }, tool_count: 6, resource_count: 2, prompt_count: 1 },
      { name: "build.local", enabled: true, approved: true, state: { type: "connecting" }, tool_count: 3, resource_count: 0, prompt_count: 0 },
      { name: "broken.remote", enabled: true, approved: true, state: { type: "failed", message: "TLS certificate rejected" }, tool_count: 2, resource_count: 0, prompt_count: 0 },
      { name: "approval.pending", enabled: true, approved: false, state: { type: "approval_required" }, tool_count: 1, resource_count: 1, prompt_count: 0 },
    ],
    mcpApprovalReview: {
      server: "docs.remote",
      transport: "streamable_http",
      endpoint: "https://docs.example/mcp",
      origin: "user configuration",
      defer_tools: true,
      fingerprint: "sha256:docs",
      previously_approved: true,
    },
  }
}

function settingsState(): RottweilerState {
  return {
    ...conversationState(),
    settings: [
      {
        key: "models.thinking.fast",
        label: "Fast thinking",
        value: "medium",
        choices: ["low", "medium", "high"],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "project.models.default",
        label: "Project default model",
        value: "gpt-5",
        choices: ["gpt-5"],
        provenance: "private project preference",
        appliesImmediately: false,
      },
      {
        key: "permissions.default",
        label: "Default approval policy",
        value: "ask",
        choices: ["ask", "allow", "deny"],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "compaction.auto",
        label: "Automatic compaction",
        value: "true",
        choices: ["true", "false"],
        provenance: "built-in",
        appliesImmediately: false,
      },
      {
        key: "budget.session_token_cap",
        label: "Session token cap",
        value: "250000",
        choices: [],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "budget.warn_at_percent",
        label: "Budget warning",
        value: "80",
        choices: [],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "mcp.servers.docs.enabled",
        label: "MCP · docs",
        value: "true",
        choices: ["true", "false"],
        provenance: "user MCP configuration",
        appliesImmediately: false,
      },
      {
        key: "ui.theme",
        label: "Theme",
        value: "kennel",
        choices: [],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "ui.keybindings.preset",
        label: "Keybinding preset",
        value: "standard",
        choices: ["standard", "vim"],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "telemetry.detail",
        label: "Telemetry detail",
        value: "minimal",
        choices: ["off", "minimal"],
        provenance: "built-in",
        appliesImmediately: false,
      },
    ],
  }
}

function toolsState(): RottweilerState {
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
    makeTool("read-app", 0, {
      name: "read",
      args: { path: "packages/tui/src/app.ts" },
      display: prepareToolDisplay({ type: "text", text: "Read 5,894 lines" }, null, { path: "packages/tui/src/app.ts" }, false), source: null,
    }),
    makeTool("search-workspace", 1, {
      name: "grep",
      args: { pattern: "ToolsWorkspaceRenderable" },
      display: prepareToolDisplay({ type: "text", text: "packages/tui/src/app.ts: ToolsWorkspaceRenderable" }, null, { pattern: "ToolsWorkspaceRenderable" }, false), source: null,
    }),
    makeTool("component-tests", 2, {
      name: "bash",
      args: { command: "bun test test/components.test.ts" },
      status: "running",
      chunks: toolOutputBuffer([{
        stream: "stdout",
        chunk: Array.from({ length: 12 }, (_, index) => `component check ${index + 1} passed`).join("\n"),
      }]),
      display: null, source: null,
      isError: null,
      timing: { kind: "open", startedAtMs, lastObservedAtMs: startedAtMs + 40_000 },
    }),
    makeTool("denied-generated-edit", 3, {
      name: "edit",
      args: { path: "generated/output.ts" },
      display: prepareToolDisplay({ type: "text", text: "permission denied for tool edit" }, null, { path: "generated/output.ts" }, true), source: null,
      isError: true,
    }),
    makeTool("write-component", 4, {
      name: "write",
      args: { path: "packages/tui/src/components/tools-workspace.ts" },
      display: prepareToolDisplay({ type: "text", text: "Wrote the retained workspace component" }, null, { path: "packages/tui/src/components/tools-workspace.ts" }, false), source: null,
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
    mode: "execute",
    provider: "openai",
    model: "gpt-5",
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
    tools: Object.fromEntries(tools.map((item) => [item.toolCallId, item])),
    queuedMessages: [
      { position: "1", content: "Run the complete suite" },
      { position: "2", content: "Inspect the direct raster" },
    ],
  }
}

function conversationState(): RottweilerState {
  const initial = createInitialState()
  const edit = tool({
    toolCallId: "edit-tool",
    name: "edit",
    args: { path: "core/cursor.rs" },
    status: "finished",
    capabilities: ["write_filesystem"],
    rationale: "Track the durable cursor independently",
    display: prepareToolDisplay({ type: "text", text: "Updated core/cursor.rs" }, null, { path: "core/cursor.rs" }, false), source: null,
    chunks: toolOutputBuffer([]),
    isError: false,
  })
  const tests = tool({
    toolCallId: "test-tool",
    name: "bash",
    args: { command: "cargo test -p rw-core" },
    status: "finished",
    capabilities: ["execute"],
    rationale: "Run the focused regression suite",
    display: prepareToolDisplay({ type: "text", text: "18 passed; 0 failed" }, null, { command: "cargo test -p rw-core" }, false), source: null,
    chunks: toolOutputBuffer([]),
    isError: false,
  })
  const read = tool({
    toolCallId: "read-tool",
    name: "read",
    args: { path: "protocol/session-log.md" },
    status: "finished",
    capabilities: ["read_filesystem"],
    rationale: "Confirm the reconnect contract",
    display: prepareToolDisplay({ type: "text", text: "184 lines" }, null, { path: "protocol/session-log.md" }, false), source: null,
    chunks: toolOutputBuffer([]),
    isError: false,
  })
  return {
    ...initial,
    connection: { phase: "connected", attempt: 0, error: null, gap: null },
    mode: "execute",
    provider: "anthropic",
    model: "sonnet-4.5",
    streamingTail: createStreamingTail({
      turnId: "2",
      text: "## What changed\n\nThe stream resumes from the last **durable** sequence, not the last delivered frame.\n\n1. `cursor.rs` tracks `durable_seq` independently\n2. `sse.ts` replays from that sequence on reattach\n3. `app.ts` drops the transport-ack fast path",
      thinking: "Two acknowledgements exist here: the transport ack and\nthe durable sequence ack. The client advances its cursor\non the transport ack, so a reconnect replays from a\nsequence the UI already consumed. Keep them separate.",
      citations: [{ uri: "protocol/session-log.md", title: "Reconnect contract" }],
      toolInvocationIds: [edit.toolCallId, tests.toolCallId, read.toolCallId],
      finished: null,
    }),
    tools: {
      [edit.toolCallId]: edit,
      [tests.toolCallId]: tests,
      [read.toolCallId]: read,
    },
    todos: { ...emptyTodos(), phase: "ready", snapshot: { items: [
      { id: "map", content: "Map the event stream", status: "completed" },
      { id: "cursor", content: "Add durable cursor", status: "in_progress" },
      { id: "tests", content: "Test reconnect replay", status: "pending" },
    ] } },
    subagentOrder: ["explore", "tests"],
    subagents: {
      explore: {
        projectionId: "explore",
        subagentId: "explore",
        parentTurnId: "2",
        task: "Map the reconnect path",
        spawnedAtMs: null,
        status: "running",
        childSessionId: "child-explore",
        lastChildSequence: "4",
        activity: "reading transport code",
        summary: null,
        touchedFileCount: 0,
        diffArtifactId: null,
      },
      tests: {
        projectionId: "tests",
        subagentId: "tests",
        parentTurnId: "2",
        task: "Check replay regressions",
        spawnedAtMs: null,
        status: "running",
        childSessionId: "child-tests",
        lastChildSequence: "3",
        activity: "running focused tests",
        summary: null,
        touchedFileCount: 0,
        diffArtifactId: null,
      },
    },
    workspaceStatus: {
      workspaceName: "Rottweiler",
      branch: "feat/tui-v2",
      changedPaths: ["core/cursor.rs", "tui/transport/sse.ts", "core/durable.rs"],
      truncated: false,
    },
    runtimeServices: [{ kind: "lsp", name: "rust-analyzer" }],
    context: {
      turn_id: "2",
      stable_prefix_hash: "visual-proof",
      used_tokens: "13200",
      usable_tokens: "32000",
      reserved_tokens: "4000",
      context_window_known: true,
      cache_breakpoints: [{ after_item_id: "policy" }],
      items: [],
    },
    cost: costSnapshot(),
    commands: [
      { name: "context", description: "Inspect assembled context", usage: "/context" },
      { name: "review", description: "Review cumulative changes", usage: "/review" },
      { name: "sessions", description: "Search and resume sessions", usage: "/sessions" },
    ],
  }
}

function tool(fields: Omit<ToolProjection, "turnId" | "invocationId" | "diff" | "callIndex" | "timing">): ToolProjection {
  return {
    ...fields,
    invocationId: fields.toolCallId,
    turnId: "2",
    diff: null,
    callIndex: 0,
    timing: { kind: "unknown" },
  }
}

function costSnapshot(): NonNullable<RottweilerState["cost"]> {
  const usage = {
    input_tokens: "12000",
    output_tokens: "1200",
    cache_read_tokens: "9000",
    cache_write_tokens: "0",
    reasoning_tokens: "512",
  }
  return {
    utc_day: "2026-08-25",
    subscription_quota: null,
    session_usage: usage,
    session_cost_micros_usd: "412000",
    session_ai_credit_micros: "0",
    session_subscription_tokens: "0",
    daily_cost_micros_usd: "412000",
    daily_ai_credit_micros: "0",
    daily_subscription_tokens: "0",
    trailing_minute_cost_micros_usd: "21000",
    trailing_minute_ai_credit_micros: "0",
    trailing_minute_subscription_tokens: "0",
    cache_hit_basis_points: 7500,
    session_cost_cap_micros_usd: null,
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
  }
}

