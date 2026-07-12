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

export function turnMarkdown(turn: Turn): string {
  const chunks: string[] = []
  for (const block of turn.blocks) {
    switch (block.type) {
      case "text":
        chunks.push(block.text)
        break
      case "thinking":
        chunks.push(`> Thinking\n> ${block.content.replaceAll("\n", "\n> ")}`)
        break
      case "citation":
        chunks.push(`[${block.title ?? block.uri}](${block.uri})`)
        break
      case "image":
        chunks.push(`🖼 image · ${block.media_type}`)
        break
      case "tool_call":
        chunks.push(`Tool call: \`${block.name}\``)
        break
      case "tool_result":
        chunks.push(toolOutputText(block.output))
        break
    }
  }
  return chunks.join("\n\n")
}

export function toolOutputText(output: ToolOutput | null): string {
  if (output === null) {
    return ""
  }
  switch (output.type) {
    case "text":
      return output.text
    case "structured":
      return boundedStructuredText(output.value)
    case "mixed":
      return output.parts
        .map((part) => {
          switch (part.type) {
            case "text":
              return part.text
            case "structured":
              return boundedStructuredText(part.value)
            case "image":
              return `[image ${part.media_type}]`
          }
        })
        .join("\n")
  }
}

function boundedStructuredText(value: unknown): string {
  return JSON.stringify(
    value,
    (key, nested: unknown) => {
      if (
        key === "diff_artifact" &&
        typeof nested === "object" &&
        nested !== null &&
        !Array.isArray(nested)
      ) {
        const artifact = nested as Record<string, unknown>
        return {
          id: artifact.id,
          base_commit: artifact.base_commit,
          touched_file_count: Array.isArray(artifact.touched_files)
            ? artifact.touched_files.length
            : 0,
          unified_diff: typeof artifact.unified_diff === "string"
            ? `[diff omitted from compact transcript · ${artifact.unified_diff.length} chars]`
            : null,
        }
      }
      if (typeof nested === "string" && nested.length > 16_384) {
        return `${nested.slice(0, 16_383)}… [${nested.length - 16_383} chars omitted]`
      }
      if (Array.isArray(nested) && nested.length > 256) {
        return [...nested.slice(0, 256), `[${nested.length - 256} items omitted]`]
      }
      return nested
    },
    2,
  )
}

export function decimal(value: string): number {
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : 0
}
