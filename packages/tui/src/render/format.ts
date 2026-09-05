import type { ContextSnapshot, Cost, CostSnapshot, ToolOutput, Turn, Usage } from "../protocol"
import type { ModelChoice } from "../state"

export const COMMAND_PREVIEW_MAX_LINES = 6

/** Shared compact command preview for transcript and approval surfaces. */
export function commandPreview(command: string, maximum = COMMAND_PREVIEW_MAX_LINES): string {
  const lines = command.split("\n")
  if (lines.length <= maximum) return command
  return [...lines.slice(0, maximum), `… ${lines.length - maximum} more lines`].join("\n")
}

/** Markdown is preserved verbatim; unsupported diagram languages remain ordinary code fences. */
export function terminalMarkdown(
  markdown: string,
  _width: number,
  _phase: "streaming" | "complete" = "complete",
): string {
  return markdown
}

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
  if (decimal(snapshot.session_subscription_quota_entries) > 0) {
    const quota = subscriptionQuota(snapshot)
    return quota ?? `${usageTokens(snapshot.session_usage)} tokens`
  }
  if (!snapshot.session_monetary_accounting_complete) {
    return `${usageTokens(snapshot.session_usage)} tokens`
  }
  if (decimal(snapshot.session_ai_credit_micros) > 0) {
    return `${(decimal(snapshot.session_ai_credit_micros) / 1_000_000).toFixed(3)} credits`
  }
  return `$${(decimal(snapshot.session_cost_micros_usd) / 1_000_000).toFixed(3)}`
}

/** Context capacity for the one-row status surface, without inventing a zero limit. */
export function formatStatusContext(snapshot: ContextSnapshot): string {
  if (!snapshot.context_window_known) {
    return `ctx ${formatTokenCount(snapshot.used_tokens)} · limit unknown`
  }
  return `ctx ${formatTokenCount(snapshot.used_tokens)}/${formatTokenCount(snapshot.usable_tokens)} (${formatPercent(snapshot.used_tokens, snapshot.usable_tokens)})`
}

/** Resolves a role alias back to the catalog's stable provider-qualified route. */
export function formatStatusModel(
  model: string,
  provider: string | null,
  choices: readonly ModelChoice[],
): string {
  if (model.includes("/")) return model
  const concrete = choices.find((choice) =>
    choice.id === model || choice.aliases.includes(model),
  )
  if (concrete !== undefined) return concrete.id
  return provider === null ? model : `${provider}/${model}`
}

/** Uses the active non-monetary route when the accounting snapshot has no priced turn yet. */
export function formatStatusSessionCost(
  snapshot: CostSnapshot | null,
  provider: string | null,
  fallbackTokens: string | null,
): string {
  if (provider === "openai_codex") {
    if (
      snapshot === null ||
      (decimal(snapshot.session_subscription_quota_entries) === 0 &&
        decimal(snapshot.session_ai_credit_micros) === 0 &&
        decimal(snapshot.session_cost_micros_usd) === 0)
    ) return "quota —"
  }
  if (
    provider === "github_copilot" &&
    (snapshot === null ||
      (decimal(snapshot.session_ai_credit_micros) === 0 &&
        decimal(snapshot.session_cost_micros_usd) === 0))
  ) return "credits —"
  return formatSessionCost(snapshot, fallbackTokens)
}

function subscriptionQuota(snapshot: CostSnapshot): string | null {
  const quota = snapshot.subscription_quota
  return quota === null ? null : `${quota.used} ${quota.unit}`
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
      const images = output.parts
        .filter((part): part is Extract<(typeof output.parts)[number], { type: "image" }> => part.type === "image")
        .map((part) => `Image attachment · ${part.media_type}`)
      const structured = output.parts
        .filter((part): part is Extract<(typeof output.parts)[number], { type: "structured" }> => part.type === "structured")
        .map((part) => structuredToolSummary(part.value))
        .filter(Boolean)
      if (structured.length > 0) return [...structured, ...images].join("\n")
      const text = output.parts
        .filter((part): part is Extract<(typeof output.parts)[number], { type: "text" }> => part.type === "text")
        .map((part) => presentableTextToolOutput(part.text))
        .filter(Boolean)
      return [...text, ...images].join("\n")
    }
  }
}

/** Last structured payload emitted for a tool invocation, with the wire envelope removed. */
export function toolStructuredData(output: ToolOutput | null): unknown | null {
  if (output === null) return null
  const values = output.type === "structured"
    ? [output.value]
    : output.type === "mixed"
      ? output.parts.flatMap((part) => part.type === "structured" ? [part.value] : [])
      : []
  return values.length === 0 ? null : unwrapToolPayload(values.at(-1))
}

/** Plain result text for tools whose user-facing result is text, never protected model framing. */
export function toolPlainText(output: ToolOutput | null): string {
  if (output === null) return ""
  const values = output.type === "text"
    ? [output.text]
    : output.type === "mixed"
      ? output.parts.flatMap((part) => part.type === "text" ? [part.text] : [])
      : []
  return values
    .map(presentableTextToolOutput)
    .filter((value) => !/^\s*<rottweiler_untrusted_[a-z0-9_]+(?:\s[^>]*)?>/i.test(value))
    .filter(Boolean)
    .join("\n")
}

function presentableTextToolOutput(source: string): string {
  const safe = source
    .replaceAll("\r\n", "\n")
    .replaceAll("\r", "\n")
    .replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, "")
  return safe
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

export function structuredToolSummary(value: unknown): string {
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
    if (prefix !== "") lines.push(`${prefix} · ${displayScalar(value)}`)
    return
  }
  for (const [key, nested] of Object.entries(value)) {
    if (isInternalField(key) || lines.length >= 32) continue
    const label = prefix === "" ? friendlyKey(key) : `${prefix} · ${friendlyKey(key)}`
    if (key === "unified_diff" && typeof nested === "string") {
      lines.push(`${label} · [diff omitted · ${nested.length} chars]`)
    } else if (isRecord(nested)) collectSummaryLines(nested, lines, label, depth + 1)
    else if (Array.isArray(nested)) lines.push(`${label} · ${nested.length} item${nested.length === 1 ? "" : "s"}`)
    else lines.push(`${label} · ${sensitiveKey(key) ? "[redacted]" : displayScalar(nested)}`)
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
    return `${value.length} item${value.length === 1 ? "" : "s"}`
  }
  return "structured result"
}

function friendlyKey(key: string): string {
  return key.replaceAll("_", " ").replace(/^./, (letter) => letter.toUpperCase())
}

function friendlyEnumValue(value: string): string {
  return value.replaceAll("_", " ").replace(/^./, (letter) => letter.toUpperCase())
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
