import { expect, test } from "bun:test"
import { mkdtemp, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { finishNativeRichProbe, MAX_NATIVE_RICH_REPORT_BYTES } from "../src/diagnostics/native-rich-report"

test("native rich settlement retains the functional failure and attempts both cleanup owners", async () => {
  const directory = await mkdtemp(join(tmpdir(), "rw-native-rich-report-"))
  const first = new Error("functional proof failed")
  const calls: string[] = []
  try {
    let thrown: unknown
    try {
      await finishNativeRichProbe(directory, { observation: "retained" }, first, {
        releaseRelay: async () => { calls.push("release"); throw new Error("relay stuck") },
        closeClient: async () => { calls.push("close"); throw new Error("runtime failed") },
        finalEvidence: () => ({ finalAllocationBytes: 9 }),
      })
    } catch (error) { thrown = error }
    expect(thrown).toBe(first)
    expect(calls).toEqual(["release", "close"])
    expect(JSON.parse(await readFile(join(directory, "native-rich.json"), "utf8"))).toMatchObject({
      observation: "retained",
      finalAllocationBytes: 9,
      failure: "functional proof failed",
      cleanupFailures: ["relay release: relay stuck", "connected client: runtime failed"],
      passed: false,
    })
  } finally { await rm(directory, { recursive: true, force: true }) }
})

test("native rich cleanup failures fail an otherwise successful proof after reporting", async () => {
  const directory = await mkdtemp(join(tmpdir(), "rw-native-rich-cleanup-"))
  const calls: string[] = []
  try {
    await expect(finishNativeRichProbe(directory, { observation: "complete" }, undefined, {
      releaseRelay: async () => { calls.push("release") },
      closeClient: async () => { calls.push("close"); throw new AggregateError([
        new Error("fatal runtime start"), new Error("renderer cleanup failed"),
      ], "connected probe cleanup failed") },
      finalEvidence: () => ({ finalAllocationBytes: 0 }),
    })).rejects.toThrow("settlement failed")
    expect(calls).toEqual(["release", "close"])
    expect(JSON.parse(await readFile(join(directory, "native-rich.json"), "utf8"))).toMatchObject({
      failure: null,
      cleanupFailures: ["connected client: fatal runtime start", "connected client: renderer cleanup failed"],
      passed: false,
    })
  } finally { await rm(directory, { recursive: true, force: true }) }
})

test("native rich report overflow publishes a bounded failure report", async () => {
  const directory = await mkdtemp(join(tmpdir(), "rw-native-rich-overflow-"))
  try {
    await expect(finishNativeRichProbe(directory,
      { observation: "x".repeat(MAX_NATIVE_RICH_REPORT_BYTES) }, undefined, {
        releaseRelay: async () => {},
        closeClient: async () => {},
        finalEvidence: () => ({ finalAllocationBytes: 0 }),
      })).rejects.toThrow("settlement failed")
    const encoded = await readFile(join(directory, "native-rich.json"))
    expect(encoded.byteLength).toBeLessThanOrEqual(MAX_NATIVE_RICH_REPORT_BYTES)
    expect(JSON.parse(encoded.toString())).toMatchObject({
      failure: null,
      cleanupFailures: [],
      reportFailure: `native rich report exceeds ${MAX_NATIVE_RICH_REPORT_BYTES} bytes`,
      passed: false,
    })
  } finally { await rm(directory, { recursive: true, force: true }) }
})
