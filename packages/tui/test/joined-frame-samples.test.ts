import { expect, test } from "bun:test"
import { FrameSamples } from "../src/diagnostics/connected/frame-samples"

const frame = { phase: "streaming", scheduledMs: 0, startedMs: 0, computeMs: 1, inputMs: 2, retainedBytes: 32 }
test("joined frame evidence retains stalls and samples without trimming", () => {
  const samples = new FrameSamples(3)
  samples.record(frame)
  samples.record({ ...frame, scheduledMs: 16, startedMs: 66, computeMs: 100, inputMs: 110 })
  samples.record({ ...frame, scheduledMs: 82, startedMs: 82, inputMs: null })
  expect(samples.summary()).toMatchObject({ frames: 3, inputSamples: 2, computeP999Ms: 100, inputP99Ms: 110, missedCadenceSlots: 3, warmupExcluded: 0 })
  expect(samples.frames.map(value => value.ordinal)).toEqual([0, 1, 2])
  expect(() => samples.record(frame)).toThrow("capacity")
})
test("missing, nonfinite and negative observations cannot manufacture passing metrics", () => {
  const samples = new FrameSamples()
  expect(samples.summary().inputP99Ms).toBeNull()
  for (const computeMs of [NaN, Infinity, -1]) expect(() => samples.record({ ...frame, computeMs })).toThrow("invalid")
  expect(() => samples.record({ ...frame, inputMs: NaN })).toThrow("invalid")
  expect(() => samples.record({ ...frame, retainedBytes: 0.5 })).toThrow("invalid")
  expect(() => new FrameSamples(1201)).toThrow("capacity")
  expect(samples.frames).toEqual([])
})
