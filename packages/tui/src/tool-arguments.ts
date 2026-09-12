const internalKey = /^(machine_local_path|stable_prefix_hash|cache_breakpoints|item_id|projected_sequence|tool_registry|checkpoint.*|protocol_version|source|kind)$/i

/** Compact argument text never traverses nested values or all object keys. */
export function formatToolArguments(args: unknown, limit = 240): string {
  const parts: string[] = []
  if (args !== null && typeof args === "object" && !Array.isArray(args)) {
    let visited = 0
    for (const key in args) {
      if (!Object.hasOwn(args, key)) continue
      if (++visited > 8) { parts.push("…"); break }
      if (internalKey.test(key)) continue
      const label = key.slice(0, 64).replaceAll("_", " ")
      const value = /token|secret|password|authorization|api[_-]?key|credential/i.test(key)
        ? "[redacted]" : scalar(Reflect.get(args, key))
      parts.push(`${label.replace(/^./, letter => letter.toUpperCase())}=${value}`)
    }
  } else parts.push(scalar(args))
  const safe = parts.join(" · ").replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g, " ").replace(/\s+/g, " ").trim()
  return truncate(safe, limit)
}
function scalar(value: unknown): string {
  if (value === null || value === undefined) return ""
  if (typeof value === "string") return truncate(value, 160)
  if (typeof value === "number" || typeof value === "boolean" || typeof value === "bigint") return String(value)
  if (Array.isArray(value)) return `${value.length} items`
  return "structured value"
}

/** A single scalar argument is a readable subject; compound inputs retain labels. */
export function formatToolSubject(args: unknown): string {
  if (args !== null && typeof args === "object" && !Array.isArray(args)) {
    let only: string | undefined
    for (const key in args) {
      if (!Object.hasOwn(args, key)) continue
      if (only !== undefined) return formatToolArguments(args, 80)
      only = key
    }
    if (only !== undefined && !internalKey.test(only) && !/token|secret|password|authorization|api[_-]?key|credential/i.test(only)) {
      const value = Reflect.get(args, only)
      if (typeof value === "string") return truncate(scalar(value).replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g, " ").trim(), 80)
    }
  }
  return formatToolArguments(args, 80)
}

function truncate(value: string, limit: number): string {
  if (value.length <= limit) return value
  let end = Math.max(0, limit - 1)
  const last = value.charCodeAt(end - 1)
  if (last >= 0xd800 && last <= 0xdbff) end--
  return `${value.slice(0, end)}…`
}
