import { ClientAllocationOwner } from "../src/client-allocation"
import { expect, test } from "bun:test"
import { TodoController } from "../src/todo-controller"
import { emptyTodos, type TodoState } from "../src/state/todos"
import { directSessionRead } from "../src/session-reader"
import type { TodoReadResult } from "../src/protocol"

const ready = (content: string): TodoReadResult => ({ type: "ready", todos: { through: "1", snapshot: { items: [{ id: "task", content, status: "pending" }] } } })

test("task session changes retain cancelled read ownership and coalesce the next source", async () => {
  let state: TodoState = emptyTodos(), active = 0, peak = 0
  const started: string[] = [], settled: Array<(value: TodoReadResult) => void> = []
  let secondStarted!: () => void
  const second = new Promise<void>(resolve => { secondStarted = resolve })
  const controller = new TodoController({ allocations: new ClientAllocationOwner(), state: () => state, update: next => { state = next }, reader: {
    todos: async (target, _signal, allocation) => {
      allocation.admit(1024)
      active++; peak = Math.max(peak, active); started.push(target.sessionId)
      if (started.length === 2) secondStarted()
      const result = await new Promise<TodoReadResult>(resolve => settled.push(resolve))
      active--
      return result
    },
  } })
  controller.open(directSessionRead("first"))
  await Bun.sleep(0)
  controller.open(directSessionRead("discarded"))
  controller.open(directSessionRead("last"))
  expect(started).toEqual(["first"])
  settled[0]!(ready("stale"))
  await second
  expect(state.snapshot.items).toEqual([])
  expect(started).toEqual(["first", "last"])
  settled[1]!(ready("fresh"))
  await controller.settle()
  expect(state.snapshot.items[0]?.content).toBe("fresh")
  expect(peak).toBe(1)
  controller.dispose()
})

test("disposed task readers settle without publishing late results or launching queued reads", async () => {
  let state: TodoState = emptyTodos(), release!: (value: TodoReadResult) => void, reads = 0
  const controller = new TodoController({ allocations: new ClientAllocationOwner(), state: () => state, update: next => { state = next }, reader: {
    todos: async () => { reads++; return new Promise<TodoReadResult>(resolve => { release = resolve }) },
  } })
  controller.open(directSessionRead("first"))
  await Bun.sleep(0)
  controller.open(directSessionRead("second"))
  controller.dispose()
  release(ready("late"))
  await controller.settle()
  expect(reads).toBe(1)
  expect(state.snapshot.items).toEqual([])
})
