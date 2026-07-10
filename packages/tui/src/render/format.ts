import type { Cost, CostSnapshot, ToolOutput, Turn } from "../protocol"

export function formatCost(cost: Cost | null | undefined): string {
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
        ? "subscription"
        : `${cost.used}${cost.unit === undefined || cost.unit === null ? "" : ` ${cost.unit}`}`
    case "unavailable":
      return "unpriced"
  }
}

export function formatSessionCost(snapshot: CostSnapshot | null): string {
  if (snapshot === null) {
    return "$—"
  }
  return `$${(decimal(snapshot.session_cost_micros_usd) / 1_000_000).toFixed(3)}`
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
      return JSON.stringify(output.value, null, 2)
    case "mixed":
      return output.parts
        .map((part) => {
          switch (part.type) {
            case "text":
              return part.text
            case "structured":
              return JSON.stringify(part.value, null, 2)
            case "image":
              return `[image ${part.media_type}]`
          }
        })
        .join("\n")
  }
}

export function decimal(value: string): number {
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : 0
}
