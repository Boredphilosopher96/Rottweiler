import { prepareToolDisplay } from "../src/state/tool-display"
import { createStreamingTail } from "../src/state/model"
import { toolOutputBuffer } from "../src/state/display-buffer"
import { describe, expect, test } from "bun:test"

import type { ContextSnapshot, Cost, CostSnapshot, Usage } from "../src/protocol"
import {
  projectToolsWorkspace,
  projectTurnSummary,
  type ToolActivityPresentation,
} from "../src/render/tools-workspace-presentation"
import {
  createInitialState,
  type RottweilerState,
  type ToolProjection,
} from "../src/state"

const usage = {
  input_tokens: "20",
  output_tokens: "8",
  cache_read_tokens: "3",
  cache_write_tokens: "0",
  reasoning_tokens: "2",
} satisfies Usage

function tool(
  toolCallId: string,
  callIndex: number,
  extra: Partial<ToolProjection> = {},
): ToolProjection {
  return {
    toolCallId,
    invocationId: toolCallId,
    turnId: "turn-tools",
    name: "read",
    args: { path: `${toolCallId}.ts` },
    status: "finished",
    capabilities: [],
    rationale: null,
    diff: null,
    diffSource: null, chunks: toolOutputBuffer([]),
    display: prepareToolDisplay({ type: "text", text: "done" }, null, { path: `${toolCallId}.ts` }, false), source: null,
    isError: false,
    callIndex,
    timing: {
      kind: "closed",
      startedAtMs: Date.parse("2026-01-01T12:00:00.000Z"),
      finishedAtMs: Date.parse("2026-01-01T12:00:05.000Z"),
    },
    ...extra,
  }
}

function sessionContext(): ContextSnapshot {
  return {
    turn_id: "turn-tools",
    stable_prefix_hash: "stable",
    used_tokens: "100",
    usable_tokens: "1000",
    reserved_tokens: "100",
    context_window_known: true,
    cache_breakpoints: [],
    items: [],
  }
}

function sessionCost(): CostSnapshot {
  return {
    utc_day: "2026-01-01",
    subscription_quota: null,
    session_usage: usage,
    session_cost_micros_usd: "500",
    session_ai_credit_micros: "0",
    daily_cost_micros_usd: "500",
    daily_ai_credit_micros: "0",
    trailing_minute_cost_micros_usd: "500",
    trailing_minute_ai_credit_micros: "0",
    session_subscription_tokens: "0",
    daily_subscription_tokens: "0",
    trailing_minute_subscription_tokens: "0",
    cache_hit_basis_points: 0,
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

function runningState(tools: readonly ToolProjection[]): RottweilerState {
  return {
    ...createInitialState(),
    streamingTail: createStreamingTail({
      turnId: "turn-tools",
      text: "",
      thinking: "",
      citations: [],
      toolInvocationIds: tools.map((item) => item.toolCallId),
      finished: null,
    }),
    turns: {
      "turn-tools": {
        turnId: "turn-tools",
        status: "running",
        usage: null,
        cost: null,
        timing: {
          kind: "open",
          startedAtMs: Date.parse("2026-01-01T12:00:00.000Z"),
          lastObservedAtMs: Date.parse("2026-01-01T12:00:09.000Z"),
        },
      },
    },
    tools: Object.fromEntries(tools.map((item) => [item.toolCallId, item])),
  }
}

describe("Tools workspace presentation", () => {
  test("orders one turn by call index and reports exact live and denied counts", () => {
    const tools = [
      tool("sixth", 5, { name: "diagnostics" }),
      tool("first", 0, {
        name: "bash",
        args: { command: "bun test" },
        status: "running",
        display: null, source: null,
        isError: null,
        timing: {
          kind: "open",
          startedAtMs: Date.parse("2026-01-01T12:00:01.000Z"),
          lastObservedAtMs: Date.parse("2026-01-01T12:00:09.000Z"),
        },
      }),
      tool("fourth", 3, { name: "grep" }),
      tool("denied", 2, {
        name: "edit",
        args: { path: "generated/out.ts" },
        display: prepareToolDisplay({
          type: "text",
          text: "permission denied for tool edit by you; matched rule deny edit(**/generated/**)",
        }, null, { path: "generated/out.ts" }, true), source: null,
        isError: true,
      }),
      tool("second", 1, {
        name: "edit",
        status: "awaiting_approval",
        display: null, source: null,
        isError: null,
      }),
      tool("fifth", 4, { name: "background_status" }),
    ]
    const state = {
      ...runningState(tools),
      context: sessionContext(),
      cost: sessionCost(),
      queuedMessages: [
        { position: "1", content: "Run the focused suite" },
        { position: "2", content: "Then inspect the raster" },
      ],
    }

    const projected = projectToolsWorkspace(
      state,
      Date.parse("2026-01-01T12:00:12.000Z"),
    )

    expect(projected.rows.map((row) => row.key)).toEqual([
      "tool:first",
      "tool:second",
      "tool:denied",
      "tool:fourth",
      "tool:fifth",
      "tool:sixth",
    ])
    expect(projected.turn).toEqual({
      kind: "running",
      turnId: "turn-tools",
      toolCount: 6,
      liveCount: 1,
      deniedCount: 1,
      elapsed: { kind: "known", milliseconds: 12_000, label: "00:12" },
      usage: null,
      cost: null,
    })
    expect(projected.queuedMessages).toEqual(state.queuedMessages)

    const denied = projected.rows.find((row) => row.key === "tool:denied")
    expect(denied).toMatchObject({
      kind: "tool",
      outcome: {
        kind: "denied",
        label: "denied",
        reason: "Permission denied. The tool was not run.",
      },
    })
    expect(JSON.stringify(denied)).not.toContain("by you")
    expect(JSON.stringify(denied)).not.toContain("matched rule")
    expect(projected).not.toHaveProperty("diagnostics")
    expect(projected).not.toHaveProperty("background")
  })

  test("uses tail windows for live output and head windows for completed output", () => {
    const live = tool("live", 0, {
      name: "bash",
      status: "running",
      display: null, source: null,
      isError: null,
      chunks: toolOutputBuffer([{ stream: "stdout", chunk: Array.from({ length: 12 }, (_, index) => `live-${index + 1}`).join("\n") }]),
    })
    const complete = tool("complete", 1, {
      name: "generic_tool",
      display: prepareToolDisplay({ type: "text", text: Array.from({ length: 12 }, (_, index) => `done-${index + 1}`).join("\n") }, null, null, false), source: null,
    })
    const truncated = tool("truncated", 2, {
      name: "bash",
      status: "running",
      display: null, source: null,
      isError: null,
      chunks: toolOutputBuffer([{
        stream: "stdout",
        chunk: "retained-1\nretained-2\n[live tool output truncated; command output continues to drain]",
      }]),
    })

    const rows = projectToolsWorkspace(runningState([live, complete, truncated]), Date.now()).rows
    const liveOutput = (rows[0] as ToolActivityPresentation).output
    const completedOutput = (rows[1] as ToolActivityPresentation).output
    const truncatedOutput = (rows[2] as ToolActivityPresentation).output

    expect(liveOutput).toEqual({
      kind: "text",
      text: Array.from({ length: 8 }, (_, index) => `live-${index + 5}`).join("\n"),
      retainedLineCount: 12,
      visibleLineCount: 8,
      hiddenRetainedLineCount: 4,
      window: "tail",
      sourceTruncated: false,
    })
    expect(completedOutput).toMatchObject({
      kind: "text",
      text: Array.from({ length: 8 }, (_, index) => `done-${index + 1}`).join("\n"),
      retainedLineCount: 12,
      visibleLineCount: 8,
      hiddenRetainedLineCount: 4,
      window: "head",
      sourceTruncated: false,
    })
    expect(truncatedOutput).toMatchObject({
      kind: "text",
      retainedLineCount: 3,
      sourceTruncated: true,
    })
  })

  test("preserves every completed cost variant without borrowing session accounting", () => {
    const costs: readonly Cost[] = [
      { kind: "monetary", amount_micros: "1234", currency: "USD" },
      { kind: "ai_credits", credits_micros: "9000", nominal_amount_micros: null, currency: null },
      { kind: "subscription_quota", used: "42", unit: "tokens" },
      { kind: "unavailable", reason: "provider omitted cost" },
    ]

    for (const cost of costs) {
      const state: RottweilerState = {
        ...runningState([tool("done", 0)]),
        streamingTail: null,
        context: sessionContext(),
        cost: sessionCost(),
        turns: {
          "turn-tools": {
            turnId: "turn-tools",
            status: "completed",
            usage,
            cost,
            timing: {
              kind: "closed",
              startedAtMs: Date.parse("2026-01-01T12:00:00.000Z"),
              finishedAtMs: Date.parse("2026-01-01T12:00:05.000Z"),
            },
          },
        },
      }

      expect(projectTurnSummary(state, "turn-tools", Date.now())).toMatchObject({
        kind: "finished",
        usage,
        cost,
      })
    }
  })

  test("projects bounded sanitized foreground shell output as a keyed activity row", () => {
    const state: RottweilerState = {
      ...createInitialState(),
      latestShell: {
          shellId: "shell-safe",
          command: "printf hello",
          active: false,
          status: 7,
          capturedOutput: Array.from({ length: 12 }, (_, index) => `safe-${index + 1}`).join("\n"),
          outputTruncated: true,
        },

    }

    const projected = projectToolsWorkspace(state, Date.now())

    expect(projected.rows).toEqual([{
      kind: "foreground_shell",
      key: "shell:shell-safe",
      shellId: "shell-safe",
      command: "printf hello",
      active: false,
      status: 7,
      output: {
        kind: "text",
        text: Array.from({ length: 8 }, (_, index) => `safe-${index + 1}`).join("\n"),
        retainedLineCount: 12,
        visibleLineCount: 8,
        hiddenRetainedLineCount: 4,
        window: "head",
        sourceTruncated: true,
      },
    }])
    expect(JSON.stringify(projected)).not.toMatch(/[\u0000\u001b]/)
  })

  test("projects at most the authoritative foreground shell from retained history", () => {
    const shellEntry = (shellId: string, capturedOutput: string): NonNullable<RottweilerState["latestShell"]> => ({
      shellId, command: `printf ${shellId}`, active: false, status: 0, capturedOutput, outputTruncated: false,
    })
    const state: RottweilerState = {
      ...createInitialState(),
      replay: { active: true, sessionId: "shell-history", completedThrough: "9" },
      shell: {
        shellId: "shell-current",
        active: false,
        status: 0,
        capturedOutput: "current latest",
      },
      latestShell: shellEntry("shell-current", "current latest"),
    }

    const projected = projectToolsWorkspace(state, Date.now())

    expect(projected.replay).toBeTrue()
    expect(projected.rows).toHaveLength(1)
    expect(projected.rows[0]).toMatchObject({
      kind: "foreground_shell",
      key: "shell:shell-current",
      shellId: "shell-current",
      output: { kind: "text", text: "current latest" },
    })
  })
})
