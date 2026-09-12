/** Conservative JS payload accounting, bounded by bytes and traversal depth. */
export function retainedJsonBytes(value: unknown, maximum: number): number {
  let bytes = 0
  function visit(current: unknown, depth: number): void {
    if (depth > 64 || bytes > maximum) {
      bytes = maximum + 1
      return
    }
    if (typeof current === "string") bytes += 24 + current.length * 2
    else if (current === null || typeof current === "number" || typeof current === "boolean") bytes += 8
    else if (Array.isArray(current)) {
      bytes += 32 + current.length * 8
      for (const child of current) {
        if (bytes > maximum) break
        visit(child, depth + 1)
      }
    } else if (typeof current === "object") {
      bytes += 48
      for (const key in current) {
        if (!Object.hasOwn(current, key)) continue
        bytes += 48 + key.length * 2
        if (bytes > maximum) break
        visit(Reflect.get(current, key), depth + 1)
      }
    } else {
      // Payloads contain JSON data, never functions or renderer objects.
      bytes = maximum + 1
    }
  }
  visit(value, 0)
  return bytes
}
