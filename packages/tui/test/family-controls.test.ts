import { expect, test } from "bun:test"
import type { ChildControlTarget, ChildControlsSnapshot, FamilyControlsSnapshot, SessionControlsSnapshot } from "../../../protocol/types"
import { ClientAllocationOwner } from "../src/client-allocation"
import { FamilyControlsController } from "../src/family-controls"
import type { FamilyControlsReader } from "../src/family-controls-reader"
import { resolveFamilyHistory } from "../src/family-history"

const target: ChildControlTarget = { session_id: "child", ancestry: [{ subagent_id: "agent", session_id: "child" }] }
const controls: SessionControlsSnapshot = { through: "4", controls: { questions: [], approvals: [], pending_plan: null } }
const family = (revision: string, available = true): FamilyControlsSnapshot => ({ revision, children: [{ target, controls: { revision, available, through: "4", questions: 1, approvals: 0, pending_plan: false } }] })
const flush = () => new Promise<void>(resolve => setImmediate(resolve))
function cancelled(signal: AbortSignal): Promise<never> {
  return new Promise((_, reject) => { if (signal.aborted) reject(signal.reason); else signal.addEventListener("abort", () => reject(signal.reason), { once: true }) })
}

test("reconnect discovers unopened child controls without progress and restarts revision namespace", async () => {
  const owner = new ClientAllocationOwner(), after: (string | null)[] = []
  let revision = "9"
  const reader: Pick<FamilyControlsReader, "watch" | "child"> = {
    async watch(_root, cursor, signal, allocation) {
      after.push(cursor); allocation.admit(4096)
      if (cursor === null) return family(revision)
      return cancelled(signal)
    },
    async child(_root, _target, _signal, allocation) { allocation.admit(8192); return { revision, snapshot: controls } },
  }
  const applied: (SessionControlsSnapshot | null)[] = []
  const controller = new FamilyControlsController({ allocations: owner, reader, changed() {}, apply(value) { applied.push(value) } })
  controller.connect("root"); await flush()
  expect(controller.pending).toHaveLength(1)
  expect(after).toEqual([null, "9"])
  controller.select(target); await flush()
  expect(controller.ready).toBe(true)
  expect(applied.at(-1)).toBe(controls)
  controller.connect(null); revision = "1"; controller.connect("root"); await flush()
  expect(after).toEqual([null, "9", null, "1"])
  expect(controller.ready).toBe(true)
  controller.close(); await controller.settled()
  expect(owner.usage.bytes).toBe(0)
})

test("selected snapshot and action target remain owned across cancellation, replacement and teardown", async () => {
  const owner = new ClientAllocationOwner(), received = Promise.withResolvers<void>(), finish = Promise.withResolvers<ChildControlsSnapshot>()
  const reader: Pick<FamilyControlsReader, "watch" | "child"> = {
    async watch(_root, after, signal) { return after === null ? family("2") : cancelled(signal) },
    async child(_root, _target, _signal, allocation) { allocation.admit(8192); received.resolve(); return finish.promise },
  }
  const applied: (SessionControlsSnapshot | null)[] = []
  const controller = new FamilyControlsController({ allocations: owner, reader, changed() {}, apply(value) { applied.push(value) } })
  controller.connect("root"); await flush(); controller.select(target); await received.promise
  controller.close()
  expect(owner.usage.bytes).toBeGreaterThanOrEqual(8192)
  finish.resolve({ revision: "2", snapshot: controls }); await controller.settled()
  expect(applied.every(value => value === null)).toBe(true)
  expect(owner.usage.bytes).toBe(0)

  const immediate = new FamilyControlsController({ allocations: owner, reader: { ...reader, child: async () => ({ revision: "2", snapshot: controls }) }, changed() {}, apply() {} })
  immediate.connect("root"); await flush(); immediate.select(target); await flush()
  const action = Promise.withResolvers<void>()
  const response = immediate.respond({ type: "question", question_id: "question", answers: [] }, async (root, selected, revision) => {
    expect(root).toBe("root"); expect(selected).toEqual(target); expect(revision).toBe("2"); await action.promise
  })
  immediate.close(); await immediate.settled()
  expect(owner.usage.bytes).toBeGreaterThan(0)
  action.resolve(); await response
  expect(owner.usage.bytes).toBe(0)
})

test("new discovery revision invalidates action admission until the selected snapshot catches up", async () => {
  const owner = new ClientAllocationOwner(), changed = Promise.withResolvers<FamilyControlsSnapshot>(), finish = Promise.withResolvers<ChildControlsSnapshot>()
  let selectedReads = 0, watches = 0
  const controller = new FamilyControlsController({ allocations: owner, reader: {
    async watch(_root, _after, signal) { return ++watches === 1 ? family("1") : watches === 2 ? changed.promise : cancelled(signal) },
    async child() { return ++selectedReads === 1 ? { revision: "1", snapshot: controls } : finish.promise },
  }, changed() {}, apply() {} })
  controller.connect("root"); await flush(); controller.select(target); await flush()
  expect(controller.ready).toBe(true)
  changed.resolve(family("2")); await flush()
  expect(controller.ready).toBe(false)
  await expect(controller.respond({ type: "plan", decision: "approve", revisions: null }, async () => {})).rejects.toThrow("refreshing")
  finish.resolve({ revision: "2", snapshot: controls }); await flush()
  expect(controller.ready).toBe(true)
  controller.close(); await controller.settled(); expect(owner.usage.bytes).toBe(0)
})

test("selected history resolves terminal bindings through exact live target authority", async () => {
  const owner = new ClientAllocationOwner(), calls: unknown[] = []
  const nested = { session_id: "grandchild", ancestry: [...target.ancestry, { subagent_id: "nested", session_id: "grandchild" }] }
  const reader: Pick<FamilyControlsReader, "scope"> = { async scope(root, selected, _signal, allocation) {
    calls.push({ root, selected }); allocation.admit(8192)
    return { type: "ready", scope: { type: "descendant", root_session_id: root,
      ancestry: [{ subagent_id: "agent", session_id: "child", source_sequence: "10" }, { subagent_id: "nested", session_id: "grandchild", source_sequence: "20" }] } }
  } }
  const source = await resolveFamilyHistory(reader, owner, "root", nested, new AbortController().signal)
  expect(calls).toEqual([{ root: "root", selected: nested }])
  expect(source.target.scope).toMatchObject({ ancestry: [{ subagent_id: "agent", source_sequence: "10" }, { subagent_id: "nested", source_sequence: "20" }] })
  expect(owner.usage.bytes).toBeGreaterThan(0)
  source.release(); expect(owner.usage.bytes).toBe(0)
})

test("repeated rebinds retain only the latest watch while the cancelled HTTP owner settles", async () => {
  const owner = new ClientAllocationOwner(), first = Promise.withResolvers<FamilyControlsSnapshot>(), entered = Promise.withResolvers<void>()
  const roots: string[] = []
  const controller = new FamilyControlsController({ allocations: owner, reader: {
    async watch(root, _after, signal, allocation) { roots.push(root); allocation.admit(4096); if (roots.length === 1) { entered.resolve(); return first.promise } return cancelled(signal) },
    async child() { throw new Error("no selection") },
  }, changed() {}, apply() {} })
  controller.connect("first"); await entered.promise
  for (let index = 0; index < 100; index++) controller.connect(`root-${index}`)
  expect(roots).toEqual(["first"])
  expect(owner.usage.bytes).toBe(4096)
  first.resolve(family("1")); await flush()
  expect(roots).toEqual(["first", "root-99"])
  controller.close(); await controller.settled()
  expect(owner.usage.bytes).toBe(0)
})

test("scope resolution refuses a different ancestry and retains canceled decode credit until settlement", async () => {
  const owner = new ClientAllocationOwner(), signal = new AbortController()
  const pending = Promise.withResolvers<import("../../../protocol/types").ChildReadScopeResult>()
  const work = resolveFamilyHistory({ async scope(_root, _target, _signal, allocation) { allocation.admit(8192); return pending.promise } }, owner, "root", target, signal.signal)
  signal.abort()
  expect(owner.usage.bytes).toBeGreaterThan(8192)
  pending.resolve({ type: "ready", scope: { type: "descendant", root_session_id: "root", ancestry: [{ subagent_id: "agent", session_id: "child", source_sequence: "10" }] } })
  await expect(work).rejects.toThrow()
  expect(owner.usage.bytes).toBe(0)
  for (const root of ["foreign", "root"]) {
    await expect(resolveFamilyHistory({ async scope() { return { type: "ready", scope: { type: "descendant", root_session_id: root,
      ancestry: [{ subagent_id: "different", session_id: "child", source_sequence: "10" }] } } } }, owner, "root", target, new AbortController().signal)).rejects.toThrow("live ancestry")
    expect(owner.usage.bytes).toBe(0)
  }
})
