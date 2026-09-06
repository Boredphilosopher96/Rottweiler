import { expect, test } from "bun:test"
import { ClientDiagnostics, CLIENT_STAGES } from "../src/client-diagnostics"

test("fixed stage counters attribute injected delay and snapshots do not share mutable buckets", () => {
  let now = 0
  const timings = new ClientDiagnostics(() => now)
  const decode = timings.start()
  now += 0.1
  timings.finish("event_decode", decode)
  const reduce = timings.start()
  now += 25
  timings.finish("reducer", reduce, 3)
  const before = timings.snapshot()
  for (let index = 0; index < 100_000; index += 1) timings.record("history_update", 0.01, 32)
  const after = timings.snapshot()
  expect(after.stages).toHaveLength(CLIENT_STAGES.length)
  expect(before.stages.find(stage => stage.stage === "history_update")?.count).toBe(0)
  expect(after.stages.find(stage => stage.stage === "history_update")).toMatchObject({ count: 100_000, units: 3_200_000 })
  expect(after.stages.find(stage => stage.stage === "reducer")).toMatchObject({ count: 1, units: 3, totalMs: 25, maxMs: 25 })
  expect(after.stages.every(stage => stage.buckets.length === after.bucketUpperBoundsMs.length + 1)).toBeTrue()
  expect(Object.keys(after)).toEqual(["version", "bucketUpperBoundsMs", "stages"])
  expect(JSON.stringify(after).length).toBeLessThan(8192)
})

test("invalid timing samples cannot poison bounded statistics", () => {
  const timings = new ClientDiagnostics()
  timings.record("reducer", NaN)
  timings.record("reducer", Infinity)
  timings.record("reducer", -1)
  timings.record("reducer", 1, -1)
  expect(timings.snapshot().stages.every(stage => stage.count === 0)).toBeTrue()
})

test("production presentation attributes an injected slow callback separately from queue age", async () => {
  const { PresentationController } = await import("../src/presentation")
  let now = 0
  const diagnostics = new ClientDiagnostics(() => now)
  const controller = new PresentationController<string>({
    diagnostics,
    scheduler: { schedule: () => 1, cancel: () => { } },
    destroyed: () => false,
    present: () => { now += 25 },
    afterPresent: () => { },
  })
  controller.enqueue("never-log-this-payload", true)
  now += 80
  controller.flush()
  const snapshot = diagnostics.snapshot()
  expect(snapshot.stages.find(stage => stage.stage === "presentation_queue_age")).toMatchObject({ count: 1, totalMs: 80 })
  expect(snapshot.stages.find(stage => stage.stage === "presentation")).toMatchObject({ count: 1, totalMs: 25, units: 1 })
  expect(JSON.stringify(snapshot)).not.toContain("never-log-this-payload")
  controller.destroy()
})
