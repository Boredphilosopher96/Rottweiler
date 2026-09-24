import { expect, test } from "bun:test"
import { MAX_ACTIVE_CHILDREN } from "../../../protocol/types"
import { ClientAllocationOwner } from "../src/client-allocation"
import { SubagentCatalog } from "../src/subagent-catalog"
import { ClientCache } from "../src/history/cache"
import type { HistoryCacheValue } from "../src/history/controller"
import { UiCatalogController } from "../src/ui/catalog"
import { presentError } from "../src/render/errors"
import { CHILD_LIST_LIMIT_NOTICE, DRAFT_LIMIT_NOTICE, DRAFT_SWITCH_LIMIT_NOTICE, PANEL_LIMIT_NOTICE } from "../src/render/resource-copy"
import { appendSessionError } from "../src/state/errors"
import { createInitialState } from "../src/state"

function expectActionableCopy(text: string): void {
  expect(text.length).toBeLessThanOrEqual(180)
  expect(text).not.toMatch(/allocation|admission|admitted|actor count|cache|active readers|storage|budget/i)
  expect(text).toMatch(/Shorten|remove|Close|reopen/)
}

test("draft refusal copy remains actionable through the production error presenter", () => {
  for (const message of [DRAFT_LIMIT_NOTICE, DRAFT_SWITCH_LIMIT_NOTICE]) {
    const shown = presentError({ category: "protocol", code: "attachment_unavailable", message })
    expect(shown.text).toBe(message)
    expect(shown.severity).toBe("warning")
    expectActionableCopy(shown.text)
  }
})

test("a refused panel read names the recovery action without cache jargon", async () => {
  const cache = new ClientCache<HistoryCacheValue>({ bytes: 1, entries: 1 })
  let reads = 0
  const controller = new UiCatalogController({ uiCatalog: async () => { reads++; return { entries: [] } }, uiPanels: async () => { reads++; return { panels: [] } } }, cache, () => {})
  try {
    controller.open("session", "panels")
    await Promise.resolve()
    expect(controller.snapshot.error).toBe(PANEL_LIMIT_NOTICE)
    expectActionableCopy(controller.snapshot.error!)
    expect(reads).toBe(0)
  } finally { controller.close(); cache.clear() }
  expect(cache.usage.bytes).toBe(0)
})

test("child-count refusal keeps diagnostic history while showing a bounded recovery message", () => {
  const owner = new ClientAllocationOwner(), catalog = new SubagentCatalog(owner)
  let caught: Error | null = null
  try {
    catalog.replace(Array.from({ length: MAX_ACTIVE_CHILDREN + 1 }, (_, index) => ({ subagent_id: String(index), child_session_id: `child-${index}`, task: "review", agent: "reviewer", model: "fast", isolation: "shared", activity: "idle" })))
  } catch (error) { caught = error as Error }
  expect(caught).not.toBeNull()
  const error = { category: "protocol", code: "subagents_failed", message: caught!.message, retryable: true } as const
  expect(presentError(error).text).toBe(CHILD_LIST_LIMIT_NOTICE)
  expectActionableCopy(presentError(error).text)
  expect(appendSessionError(createInitialState(), error).errorHistory[0]?.message).toBe(caught!.message)
  expect(owner.usage.bytes).toBe(0)
})
