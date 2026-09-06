import { resolveRenderLib } from "@opentui/core"
import { heapStats } from "bun:jsc"

export function clientMemoryBreakdown() {
  const heap = heapStats()
  const memory = process.memoryUsage()
  return { heapSize: heap.heapSize, heapCapacity: heap.heapCapacity, objectCount: heap.objectCount,
    externalBytes: memory.external, arrayBufferBytes: memory.arrayBuffers, native: resolveRenderLib().getAllocatorStats() }
}
