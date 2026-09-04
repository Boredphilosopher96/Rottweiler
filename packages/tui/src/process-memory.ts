/** OS high-water and current resident memory expressed in bytes. */
export function observedResidentBytes(): number {
  // Pinned Bun exposes getrusage.ru_maxrss without Node's normalization:
  // bytes on Darwin, KiB on Linux. Keep the native high-water observation
  // because allocator-facing current RSS can lag released render graphs.
  const highWater = process.resourceUsage().maxRSS
  const highWaterBytes = process.platform === "darwin" ? highWater : highWater * 1024
  return Math.max(process.memoryUsage.rss(), highWaterBytes)
}
