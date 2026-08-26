import { describe, expect, test } from "bun:test"

import {
  boundSubagentState,
  childPassiveInteractionState,
  mergeComposerDraft,
  sanitizeSubagentDescriptor,
  type SubagentDescriptor,
} from "../src/subagent-state"
import { createInitialState } from "../src/state"


function descriptor(overrides: Partial<SubagentDescriptor> = {}): SubagentDescriptor {
  return {
    subagent_id: "child-1",
    child_session_id: "session-child-1",
    task: "Review the boundary",
    agent: "reviewer",
    model: "fast",
    isolation: "worktree",
    activity: "running",
    ...overrides,
  }
}


describe("subagent state boundary", () => {
  test("rejects unsafe child identifiers and bounds display metadata", () => {
    expect(sanitizeSubagentDescriptor(descriptor({ subagent_id: "bad\nid" }))).toBeNull()
    const sanitized = sanitizeSubagentDescriptor(descriptor({
      task: "x".repeat(1_024),
      agent: "a".repeat(256),
      model: "m".repeat(512),
    }))
    expect(sanitized?.task.length).toBeLessThanOrEqual(512)
    expect(sanitized?.agent.length).toBeLessThanOrEqual(128)
    expect(sanitized?.model.length).toBeLessThanOrEqual(256)
  })

  test("restores rejected composer content without duplicating attachments", () => {
    const attachment = {
      name: "lib.rs",
      source_path: "src/lib.rs",
      media_type: "text/plain",
      data: { type: "text" as const, content: "fn main() {}" },
    }
    const restored = mergeComposerDraft(
      { content: "new draft", attachments: [attachment] },
      "rejected draft",
      [attachment],
    )
    expect(restored.content).toBe("rejected draft\nnew draft")
    expect(restored.attachments).toEqual([attachment])
  })

  test("bounds child projections and removes passive mutation prompts", () => {
    const state = createInitialState()
    const turns = Object.fromEntries(
      Array.from({ length: 600 }, (_, index) => [
        `turn-${index}`,
        {
          turnId: `${index}`,
          status: "completed" as const,
          usage: null,
          cost: null,
          timing: { kind: "unknown" as const },
        },
      ]),
    )
    const bounded = boundSubagentState({ ...state, turns })
    expect(Object.keys(bounded.turns)).toHaveLength(512)

    const passive = childPassiveInteractionState({
      ...state,
      tools: {
        approval: {
          toolCallId: "approval",
          turnId: "1",
          name: "write",
          status: "awaiting_approval",
          args: {},
          capabilities: ["write_filesystem"],
          rationale: null,
          diff: null,
          chunks: [],
          output: null,
          isError: null,
          callIndex: 0,
          timing: { kind: "unknown" },
        },
      },
    })
    expect(passive.tools).toEqual({})
    expect(passive.questions).toEqual({})
    expect(passive.pendingPlan).toBeNull()
  })
})
