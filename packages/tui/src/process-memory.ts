import { constants } from "node:os"

/** OS high-water and current resident memory expressed in bytes. */
export function observedResidentBytes(): number {
  // Pinned Bun exposes getrusage.ru_maxrss without Node's normalization:
  // bytes on Darwin, KiB on Linux. Keep the native high-water observation
  // because allocator-facing current RSS can lag released render graphs.
  const highWater = process.resourceUsage().maxRSS
  const highWaterBytes = process.platform === "darwin" ? highWater : highWater * 1024
  return Math.max(currentResidentBytes(), highWaterBytes)
}

/** Each observation is fresh; interrupted OS reads receive at most three attempts. */
export function currentResidentBytes(): number { return readMemoryUsage(() => process.memoryUsage.rss()) }
export function currentMemoryUsage(): NodeJS.MemoryUsage { return readMemoryUsage(() => process.memoryUsage()) }

function readMemoryUsage<T>(read: () => T): T {
  for (let attempt = 0; ; attempt++) {
    try { return read() } catch (error) {
      if (attempt === 2 || typeof error !== "object" || error === null || !("syscall" in error)
        || error.syscall !== "memoryUsage" || !("errno" in error) || error.errno !== constants.errno.EINTR) throw error
    }
  }
}
