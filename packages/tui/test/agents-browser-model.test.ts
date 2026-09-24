import { expect, test } from "bun:test"
import { createAgentsBrowserModel, type AgentsBrowserAction } from "../src/agents-browser"
import type { ListDetailItemRow } from "../src/components/list-detail"
import { createInitialState } from "../src/state"

test("agents detail names the model and omits an unknown cost", () => {
  const state = {
    ...createInitialState(),
    models: [{
      id: "openai_codex/gpt-5.6-terra", displayName: "GPT-5.6-Terra", provider: "openai_codex", aliases: [],
      current: true, available: true, status: null, vision: false, thinking: true, toolCalling: true, contextTokens: "272000",
    }],
    subagentOrder: ["child-1"],
    subagents: { "child-1": {
      projectionId: "child-1", subagentId: "child-1", parentTurnId: "1", task: "Locate calc.py", spawnedAtMs: null,
      status: "completed" as const, childSessionId: "session-child-1", lastChildSequence: "4", activity: null,
      summary: "Found it", touchedFileCount: 0, cost: { kind: "unavailable" as const, reason: "no price" }, diffArtifactId: null,
    } },
  }
  const model = createAgentsBrowserModel({
    state,
    catalog: [{ subagent_id: "child-1", child_session_id: "session-child-1", task: "Locate calc.py", agent: "explore",
      model: "openai_codex/gpt-5.6-terra", isolation: "shared", activity: "idle" }],
    pending: [], foreground: null, finishedInStrip: 1, loading: false, error: null, query: "", selectedId: null,
  })
  const row = model.rows.find((candidate): candidate is ListDetailItemRow<AgentsBrowserAction> => candidate.kind === "item")!
  expect(row.label).toBe("✓ explore · Locate calc.py")
  expect(row.detail.meta).toBe("GPT-5.6-Terra · shared")
  expect(row.detail.description).not.toContain("cost")
  expect(model.rows.map(candidate => candidate.id)).not.toContain("agents.hide-finished")
  expect(model.status).toContain("Ctrl+D clear finished from strip")
})
