import { expect, test } from "bun:test"
import { MAX_ACTIVE_CHILDREN, MAX_CHILD_TASK_PREVIEW_BYTES } from "../../../protocol/types"
import { ClientAllocationOwner } from "../src/client-allocation"
import { SubagentCatalog } from "../src/subagent-catalog"
import type { SubagentDescriptor } from "../src/subagent-state"

function row(id: string, task = "inspect"): SubagentDescriptor {
  return { subagent_id: id, child_session_id: `session-${id}`, task, agent: "reviewer", model: "fast", isolation: "shared", activity: "idle" }
}

test("the child catalog owns bounded preview bytes independently of source replies", () => {
  const owner = new ClientAllocationOwner(), catalog = new SubagentCatalog(owner)
  const task = `x${"\u0301".repeat(500_000)}`
  catalog.replace([row("one", task)])
  expect(Buffer.byteLength(catalog.values[0]!.task)).toBeLessThanOrEqual(MAX_CHILD_TASK_PREVIEW_BYTES)
  expect(owner.usage.domains.children).toBeGreaterThan(0)
  expect(owner.usage.bytes).toBeLessThan(16 * 1024)
  const first = owner.usage.bytes
  for (let index = 0; index < 100; index++) catalog.activity("one", index % 2 === 0 ? "running" : "idle")
  expect(owner.usage.bytes).toBe(first)
  catalog.remove("one")
  expect(catalog.values).toHaveLength(0)
  catalog.clear(); expect(owner.usage.bytes).toBe(0)
})

test("catalog refusal preserves the complete prior binding and its exact charge", () => {
  const owner = new ClientAllocationOwner(), catalog = new SubagentCatalog(owner)
  catalog.replace([row("one")])
  const before = owner.usage.bytes, values = catalog.values
  const pressure = owner.reserve("children", owner.limits.children - before)
  expect(() => catalog.replace([row("two")])).toThrow("admission")
  expect(catalog.values).toBe(values)
  expect(owner.usage.bytes).toBe(before + pressure.bytes)
  pressure.release()
  expect(() => catalog.replace(Array.from({ length: MAX_ACTIVE_CHILDREN + 1 }, (_, index) => row(String(index))))).toThrow("actor count")
  expect(catalog.values).toBe(values)
  catalog.clear(); expect(owner.usage.bytes).toBe(0)
})
