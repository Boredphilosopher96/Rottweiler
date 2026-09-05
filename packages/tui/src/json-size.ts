/** Exact UTF-8 JSON size without creating the encoded string or byte buffer. */
export function jsonEncodedBytes(value: unknown, maximum: number): number {
  let bytes = 0
  const full = () => bytes > maximum
  function string(value: string): void {
    bytes += 2
    for (let index = 0; index < value.length && !full(); index++) {
      const unit = value.charCodeAt(index)
      if (unit === 34 || unit === 92 || unit === 8 || unit === 9 || unit === 10 || unit === 12 || unit === 13) bytes += 2
      else if (unit < 32) bytes += 6
      else if (unit < 128) bytes++
      else if (unit < 2048) bytes += 2
      else if (unit >= 0xd800 && unit <= 0xdbff) {
        const next = value.charCodeAt(index + 1)
        if (next >= 0xdc00 && next <= 0xdfff) { bytes += 4; index++ }
        else bytes += 6
      } else bytes += unit >= 0xdc00 && unit <= 0xdfff ? 6 : 3
    }
  }
  function visit(current: unknown, depth: number): void {
    if (full()) return
    if (depth > 64) { bytes = maximum + 1; return }
    if (typeof current === "string") string(current)
    else if (current === null) bytes += 4
    else if (typeof current === "boolean") bytes += current ? 4 : 5
    else if (typeof current === "number") bytes += Number.isFinite(current) ? String(current).length : 4
    else if (Array.isArray(current)) {
      bytes += 2
      for (let index = 0; index < current.length && !full(); index++) {
        if (index > 0) bytes++
        visit(current[index] === undefined ? null : current[index], depth + 1)
      }
    } else if (typeof current === "object") {
      bytes += 2
      let count = 0
      for (const key in current) {
        if (full()) break
        if (!Object.hasOwn(current, key)) continue
        const child = Reflect.get(current, key)
        if (child === undefined) continue
        if (count++ > 0) bytes++
        string(key)
        bytes++
        visit(child, depth + 1)
      }
    } else bytes = maximum + 1
  }
  visit(value, 0)
  return Math.min(bytes, maximum + 1)
}
