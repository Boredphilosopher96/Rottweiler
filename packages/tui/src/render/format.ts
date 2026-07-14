import type { Cost, CostSnapshot, ToolOutput, Turn, Usage } from "../protocol"

export function formatCost(cost: Cost | null | undefined, usage?: Usage | null): string {
  if (cost === null || cost === undefined) {
    return "—"
  }
  switch (cost.kind) {
    case "monetary":
      return `${cost.currency.toUpperCase()} ${(decimal(cost.amount_micros) / 1_000_000).toFixed(4)}`
    case "ai_credits":
      return `${(decimal(cost.credits_micros) / 1_000_000).toFixed(3)} credits`
    case "subscription_quota":
      return cost.used === undefined || cost.used === null
        ? usage === undefined || usage === null
          ? "subscription"
          : `${usageTokens(usage)} tokens`
        : `${cost.used}${cost.unit === undefined || cost.unit === null ? "" : ` ${cost.unit}`}`
    case "unavailable":
      return "unpriced"
  }
}

export function formatSessionCost(
  snapshot: CostSnapshot | null,
  fallbackTokens: string | null = null,
): string {
  if (snapshot === null) {
    return fallbackTokens === null ? "$—" : `${fallbackTokens} tokens`
  }
  if (!snapshot.session_monetary_accounting_complete) {
    return `${usageTokens(snapshot.session_usage)} tokens`
  }
  if (decimal(snapshot.session_subscription_quota_entries) > 0) {
    const quota = subscriptionQuota(snapshot)
    return quota ?? `${usageTokens(snapshot.session_usage)} tokens`
  }
  if (decimal(snapshot.session_ai_credit_micros) > 0) {
    return `${(decimal(snapshot.session_ai_credit_micros) / 1_000_000).toFixed(3)} credits`
  }
  return `$${(decimal(snapshot.session_cost_micros_usd) / 1_000_000).toFixed(3)}`
}

function subscriptionQuota(snapshot: CostSnapshot): string | null {
  const values = snapshot.turns
    .map((turn) => turn.cost)
    .filter((cost): cost is Extract<Cost, { kind: "subscription_quota" }> =>
      cost.kind === "subscription_quota" && cost.used !== undefined && cost.used !== null,
    )
  if (values.length === 0) return null
  const units = new Set(values.map((cost) => cost.unit ?? "quota"))
  if (units.size !== 1) return null
  const used = values.reduce((sum, cost) => sum + decimal(cost.used ?? "0"), 0)
  return `${used} ${values[0]?.unit ?? "quota"}`
}

function usageTokens(usage: Usage): string {
  return (
    decimal(usage.input_tokens) +
    decimal(usage.output_tokens)
  ).toFixed(0)
}

export function formatPercent(numerator: string, denominator: string): string {
  const used = decimal(numerator)
  const total = decimal(denominator)
  if (total <= 0) {
    return "—%"
  }
  return `${Math.min(999, Math.round((used / total) * 100))}%`
}

export function formatTokenCount(value: string): string {
  const tokens = decimal(value)
  if (!Number.isFinite(tokens)) return "—"
  if (tokens < 1_000) return tokens.toFixed(0)
  if (tokens < 10_000) return `${(tokens / 1_000).toFixed(1)}k`
  if (tokens < 1_000_000) return `${Math.round(tokens / 1_000)}k`
  return `${(tokens / 1_000_000).toFixed(1)}m`
}

export function turnMarkdown(turn: Turn): string {
  const chunks: string[] = []
  for (const block of turn.blocks) {
    switch (block.type) {
      case "text":
        chunks.push(block.text)
        break
      case "thinking":
        // Reasoning has a dedicated compact, expandable presentation.
        break
      case "citation":
        chunks.push(`[${block.title ?? block.uri}](${block.uri})`)
        break
      case "image":
        chunks.push(`🖼 image · ${block.media_type}`)
        break
      case "tool_call":
      case "tool_result":
        // Tool activity has a dedicated compact, expandable card. Rendering
        // provider-facing tool blocks here as transcript Markdown duplicates
        // every result and exposes structured protocol payloads to users.
        break
    }
  }
  return chunks.join("\n\n")
}

/** Provider reasoning suitable for user presentation, excluding encrypted placeholders. */
export function turnReasoningMarkdown(turn: Turn): string {
  return turn.blocks
    .filter((block): block is Extract<Turn["blocks"][number], { type: "thinking" }> =>
      block.type === "thinking",
    )
    .map((block) => block.content.replaceAll("[REDACTED]", "").trim())
    .filter(Boolean)
    .join("\n\n")
}

export function toolOutputText(output: ToolOutput | null): string {
  if (output === null) {
    return ""
  }
  switch (output.type) {
    case "text":
      return presentableTextToolOutput(output.text)
    case "structured":
      return structuredToolSummary(output.value)
    case "mixed": {
      const text = output.parts
        .filter((part): part is Extract<(typeof output.parts)[number], { type: "text" }> => part.type === "text")
        .map((part) => presentableTextToolOutput(part.text))
        .filter(Boolean)
      const images = output.parts
        .filter((part): part is Extract<(typeof output.parts)[number], { type: "image" }> => part.type === "image")
        .map((part) => `Image attachment · ${part.media_type}`)
      if (text.length > 0) return [...text, ...images].join("\n")
      const structured = output.parts
        .filter((part): part is Extract<(typeof output.parts)[number], { type: "structured" }> => part.type === "structured")
        .map((part) => structuredToolSummary(part.value))
        .filter(Boolean)
      return [...structured, ...images].join("\n")
    }
  }
}

function presentableTextToolOutput(source: string): string {
  const safe = source
    .replaceAll("\r\n", "\n")
    .replaceAll("\r", "\n")
    .replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, "")
  const trimmed = safe.trim()
  // Some provider adapters serialize a structured ToolOutput as text. Decode
  // only recognizable protocol envelopes; a file read containing ordinary
  // JSON remains the file content the user asked to inspect.
  if ((trimmed.startsWith("{") || trimmed.startsWith("[")) && trimmed.length <= 512 * 1024) {
    try {
      const parsed: unknown = JSON.parse(trimmed)
      if (looksLikeToolEnvelope(parsed)) return structuredToolSummary(parsed)
    } catch {
      // User-authored or partial JSON is ordinary output, not a UI failure.
    }
  }
  return safe
}

function looksLikeToolEnvelope(value: unknown): boolean {
  if (!isRecord(value)) return false
  return "data" in value || "truncated" in value || "machine_local_path" in value ||
    "stable_prefix_hash" in value || "tool_call_id" in value
}

/** Compact, terminal-safe tool arguments for approval and activity surfaces. */
export function formatToolArguments(args: unknown, limit = 240): string {
  let rendered: string
  try {
    rendered = argumentPairs(args)
  } catch {
    rendered = "[unavailable]"
  }
  const safe = rendered
    .replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g, " ")
    .replace(/\s+/g, " ")
    .trim()
  return safe.length <= limit ? safe : `${safe.slice(0, Math.max(1, limit - 1))}…`
}

function argumentPairs(value: unknown): string {
  if (value === null || value === undefined) return ""
  if (typeof value !== "object" || Array.isArray(value)) return displayScalar(value)
  return Object.entries(value)
    .filter(([key]) => !isInternalField(key))
    .map(([key, nested]) => `${friendlyKey(key)}=${sensitiveKey(key) ? "[redacted]" : displayScalar(nested)}`)
    .join(" · ")
}

function structuredToolSummary(value: unknown): string {
  const payload = unwrapToolPayload(value)
  if (isRecord(payload)) {
    if (Array.isArray(payload.paths)) {
      const paths = payload.paths.filter((path): path is string => typeof path === "string")
      return paths.length === 0 ? "No matching files." : boundedSummaryRows(paths, "files")
    }
    if (Array.isArray(payload.entries)) {
      const entries = payload.entries.flatMap((entry) => {
        if (!isRecord(entry) || typeof entry.path !== "string") return []
        return [`${typeof entry.kind === "string" ? friendlyEnumValue(entry.kind) : "Item"}  ${entry.path}`]
      })
      return entries.length === 0 ? "No entries." : boundedSummaryRows(entries, "entries")
    }
    if (Array.isArray(payload.matches)) {
      const matches = payload.matches.flatMap((match) => {
        if (!isRecord(match) || typeof match.path !== "string") return []
        const line = typeof match.line === "number" ? `:${match.line}` : ""
        const text = typeof match.text === "string" ? `  ${match.text}` : ""
        return [`${match.path}${line}${text}`]
      })
      return matches.length === 0 ? "No matches." : boundedSummaryRows(matches, "matches")
    }
  }
  const lines: string[] = []
  collectSummaryLines(payload, lines, "", 0)
  return lines.length === 0 ? "Completed." : boundedSummaryRows(lines, "details")
}

function boundedSummaryRows(rows: readonly string[], noun: string, maximum = 24): string {
  if (rows.length <= maximum) return rows.join("\n")
  return [...rows.slice(0, maximum), `… ${rows.length - maximum} more ${noun}`].join("\n")
}

function collectSummaryLines(value: unknown, lines: string[], prefix: string, depth: number): void {
  if (lines.length >= 32 || depth > 2) return
  if (!isRecord(value)) {
    if (prefix !== "") lines.push(`${prefix}: ${displayScalar(value)}`)
    return
  }
  for (const [key, nested] of Object.entries(value)) {
    if (isInternalField(key) || lines.length >= 32) continue
    const label = prefix === "" ? friendlyKey(key) : `${prefix} · ${friendlyKey(key)}`
    if (key === "unified_diff" && typeof nested === "string") {
      lines.push(`${label}: [diff omitted · ${nested.length} chars]`)
    } else if (isRecord(nested)) collectSummaryLines(nested, lines, label, depth + 1)
    else if (Array.isArray(nested)) lines.push(`${label}: ${nested.length} item${nested.length === 1 ? "" : "s"}`)
    else lines.push(`${label}: ${sensitiveKey(key) ? "[redacted]" : displayScalar(nested)}`)
  }
}

function unwrapToolPayload(value: unknown): unknown {
  if (!isRecord(value)) return value
  if ("data" in value) return value.data
  return value
}

function displayScalar(value: unknown): string {
  if (value === null) return "none"
  if (typeof value === "string") return value.length <= 160 ? value : `${value.slice(0, 159)}…`
  if (typeof value === "number" || typeof value === "boolean" || typeof value === "bigint") return String(value)
  if (Array.isArray(value)) {
    const shown = value.slice(0, 8).map(displayScalar).join(", ")
    return value.length <= 8 ? shown : `${shown}, …`
  }
  return "details available"
}

function friendlyKey(key: string): string {
  return key.replaceAll("_", " ").replace(/^./, (letter) => letter.toUpperCase())
}

function friendlyEnumValue(value: string): string {
  return value.replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase())
}

function sensitiveKey(key: string): boolean {
  return /token|secret|password|authorization|api[_-]?key|credential/i.test(key)
}

function isInternalField(key: string): boolean {
  return /^(machine_local_path|stable_prefix_hash|cache_breakpoints|item_id|projected_sequence|tool_registry|checkpoint.*|protocol_version|source|kind)$/i.test(key)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

export function decimal(value: string): number {
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : 0
}
