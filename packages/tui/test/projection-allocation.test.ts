import { MAX_PENDING_TOOL_INVOCATIONS } from "../../../protocol/types"
import { expect, test } from "bun:test"
import { CLIENT_ALLOCATION_LIMITS, ClientAllocationOwner } from "../src/client-allocation"
import { ProjectionAllocations, ProjectionGraph } from "../src/state/allocation"
import { createInitialState } from "../src/state"
import { EMPTY_TOOL_OUTPUT } from "../src/state/display-buffer"

test("mounted root and child stay charged until a complete rebind, with identical revisions charged once", () => {
  const owner = new ClientAllocationOwner(), models = new ProjectionAllocations(owner)
  const first = models.root, firstBytes = owner.usage.bytes
  models.presented()
  expect(owner.usage.bytes).toBe(firstBytes)
  const next = { ...first, model: "another-model" }
  models.set("root", next)
  const combined = owner.usage.bytes
  expect(combined).toBeGreaterThan(firstBytes)
  expect(combined).toBeLessThan(firstBytes * 2)
  models.set("child", first)
  expect(owner.usage.bytes).toBe(combined)
  models.presented()
  expect(owner.usage.bytes).toBe(combined)
  models.set("child", null)
  expect(owner.usage.bytes).toBe(combined)
  models.presented()
  expect(owner.usage.bytes).toBeLessThan(combined)
  const retained = owner.usage.bytes
  const inFlight = models.retain()
  models.set("root", { ...next, model: "newest" }); models.presented()
  expect(owner.usage.bytes).toBeGreaterThan(retained)
  inFlight.release(); models.dispose()
  expect(owner.usage.bytes).toBe(0)
})

test("refused incoming projection preserves the exact mounted unresolved question", () => {
  const initial = createInitialState()
  const question = { questionId: "q", turnId: "1", questions: [], answered: false, answers: null }
  const pending = { ...initial, questions: { q: question } }
  const max = 100_000
  const owner = new ClientAllocationOwner({ ...CLIENT_ALLOCATION_LIMITS, live: max }, max)
  const models = new ProjectionAllocations(owner)
  models.set("root", pending); models.presented()
  const before = owner.usage.bytes
  expect(() => models.set("root", { ...pending, model: "x".repeat(max) })).toThrow("admission")
  expect(models.root).toBe(pending)
  expect(models.root.questions.q).toBe(question)
  expect(owner.usage.bytes).toBe(before)
  models.dispose()
})

test("immutable sizing reuses large argument graphs and accounts chunk chains in constant work", () => {
  const owner = new ClientAllocationOwner(), graph = new ProjectionGraph(owner)
  const args = { nested: Array.from({ length: 1000 }, (_, i) => ({ id: String(i), value: "x".repeat(128) })) }
  let chunks = EMPTY_TOOL_OUTPUT
  let state = { args, chunks }
  graph.replace([state], [])
  const initial = graph.visitedObjects
  for (let i = 0; i < 1000; i++) {
    chunks = chunks.append({ stream: "stdout", chunk: "a\n" })
    const next = { args, chunks }
    graph.replace([next], [state]); state = next
  }
  expect(graph.visitedObjects - initial).toBe(2000)
  const before = graph.visitedObjects
  graph.replace([state], [state])
  expect(graph.visitedObjects).toBe(before)
  graph.replace([], [state])
  expect(graph.retainedObjects).toBe(0)
  expect(owner.usage.bytes).toBe(0)
  graph.dispose()
})

test("a saturated invocation set admits its next immutable revision without duplicating chunk history", () => {
  const owner = new ClientAllocationOwner(), graph = new ProjectionGraph(owner)
  const streams = Array.from({ length: MAX_PENDING_TOOL_INVOCATIONS }, (_, tool) => {
    let buffer = EMPTY_TOOL_OUTPUT
    for (let chunk = 0; chunk < 1024; chunk++) buffer = buffer.append({ stream: "stdout", chunk: `${tool}:${chunk}:`.padEnd(63, "x") + "\n" })
    buffer.preview()
    return buffer
  })
  const first = { streams }
  graph.replace([first], [])
  const bytes = owner.usage.bytes, visited = graph.visitedObjects
  const next = { streams: [...streams] }
  next.streams[0] = streams[0]!.append({ stream: "stdout", chunk: "omitted" })
  graph.replace([next], [])
  expect(owner.usage.bytes - bytes).toBeLessThan(4096)
  expect(graph.visitedObjects - visited).toBe(3)
  graph.replace([], [first])
  expect(graph.retainedObjects).toBeLessThan(300)
  graph.replace([], [next])
  expect(graph.retainedObjects).toBe(0)
  expect(owner.usage.bytes).toBe(0)
})

test("repeated rejected stream appends leave no allocation debt in the retained prefix", () => {
  const owner = new ClientAllocationOwner({ ...CLIENT_ALLOCATION_LIMITS, live: 2_000_000 }, 2_000_000)
  const graph = new ProjectionGraph(owner)
  const buffer = EMPTY_TOOL_OUTPUT.append({ stream: "stdout", chunk: "original" })
  const first = { buffer }
  graph.replace([first], [])
  const retained = owner.usage.bytes, objects = graph.retainedObjects
  const blocker = owner.reserve("live", owner.maximumBytes - retained - 4096)
  for (let index = 0; index < 100; index++) {
    const rejected = { buffer: buffer.append({ stream: "stderr", chunk: "x".repeat(8192) }) }
    expect(() => graph.replace([rejected], [first])).toThrow("admission")
    expect(owner.usage.bytes).toBe(retained + blocker.bytes)
    expect(graph.retainedObjects).toBe(objects)
  }
  blocker.release()
  const accepted = { buffer: buffer.append({ stream: "stdout", chunk: "accepted" }) }
  graph.replace([accepted], [first])
  const freshOwner = new ClientAllocationOwner(), fresh = new ProjectionGraph(freshOwner)
  fresh.replace([accepted], [])
  expect(graph.bytes).toBe(fresh.bytes)
  graph.dispose(); fresh.dispose()
  expect(owner.usage.bytes).toBe(0)
})

test("divergent source branches release only their exclusive nodes", () => {
  const owner = new ClientAllocationOwner(), graph = new ProjectionGraph(owner)
  const prefix = EMPTY_TOOL_OUTPUT.append({ stream: "stdout", chunk: "shared" })
  const a = { buffer: prefix.append({ stream: "stdout", chunk: "a".repeat(100) }) }
  const b = { buffer: prefix.append({ stream: "stderr", chunk: "b".repeat(200) }) }
  const c = { buffer: prefix.append({ stream: "stderr", chunk: "c".repeat(300) }) }
  graph.replace([a, b, c], [])
  const all = graph.bytes
  graph.replace([], [b])
  expect(graph.bytes).toBeLessThan(all)
  graph.replace([], [a])
  const freshOwner = new ClientAllocationOwner(), fresh = new ProjectionGraph(freshOwner)
  fresh.replace([c], [])
  expect(graph.bytes).toBe(fresh.bytes)
  graph.replace([], [c])
  expect(graph.bytes).toBe(0)
  expect(graph.retainedObjects).toBe(0)
  fresh.dispose(); graph.dispose()
})

test("staging refusal leaves the existing graph and shared allocation ledger unchanged", () => {
  const owner = new ClientAllocationOwner({ ...CLIENT_ALLOCATION_LIMITS, live: 8192 }, 8192)
  const graph = new ProjectionGraph(owner), first = { pending: "keep" }
  graph.replace([first], [])
  const bytes = graph.bytes, visited = graph.visitedObjects
  const oversized = { values: Array.from({ length: 10_000 }, () => ({ field: "large" })) }
  expect(() => graph.replace([oversized], [first])).toThrow("admission")
  expect(graph.bytes).toBe(bytes)
  expect(owner.usage.bytes).toBe(bytes)
  expect(graph.visitedObjects - visited).toBeLessThan(10)
  graph.replace([], [first])
  expect(owner.usage.bytes).toBe(0)
})
