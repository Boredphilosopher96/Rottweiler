import type { ContextSnapshot, Cost, CostSnapshot, Turn, Usage } from "../protocol"
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

export { formatToolArguments } from "../tool-arguments"

export function decimal(value: string): number {
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : 0
}
