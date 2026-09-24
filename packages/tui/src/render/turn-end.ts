import type { Cost, TurnStatus, Usage } from "../protocol"
import { decimal, formatTokenCount } from "./format"

export interface TurnEndLine {
  readonly text: string
  readonly tone: "muted" | "warning" | "error"
}

/**
 * The closing line of a turn, shared by the live tail and restored history.
 * A normal completion is silent unless usage or a real price is known; other
 * endings always state what stopped the turn.
 */
export function turnEndLine(status: TurnStatus, cost: Cost | null, usage: Usage | null): TurnEndLine | null {
  switch (status) {
    case "interrupted": return { text: "Interrupted", tone: "warning" }
    case "failed": return { text: "Failed", tone: "error" }
    case "max_turns": return { text: "Stopped · turn limit reached", tone: "warning" }
    case "doom_loop": return { text: "Stopped · repeated tool calls detected", tone: "warning" }
    case "budget_exceeded": return { text: "Stopped · budget limit reached", tone: "warning" }
    case "completed": {
      const parts = [tokens(usage), price(cost)].filter((part): part is string => part !== null)
      return parts.length === 0 ? null : { text: parts.join(" · "), tone: "muted" }
    }
  }
}

function tokens(usage: Usage | null): string | null {
  if (usage === null) return null
  const total = decimal(usage.input_tokens) + decimal(usage.output_tokens)
  return total > 0 ? `${formatTokenCount(String(total))} tokens` : null
}

function price(cost: Cost | null): string | null {
  if (cost === null) return null
  switch (cost.kind) {
    case "monetary": {
      const amount = decimal(cost.amount_micros) / 1_000_000
      if (amount <= 0) return null
      const symbol = cost.currency.toUpperCase() === "USD" ? "$" : `${cost.currency.toUpperCase()} `
      return `${symbol}${amount < 0.01 ? amount.toFixed(4) : amount.toFixed(2)}`
    }
    case "ai_credits": {
      const credits = decimal(cost.credits_micros) / 1_000_000
      return credits > 0 ? `${credits.toFixed(3)} credits` : null
    }
    case "subscription_quota":
      return cost.used === undefined || cost.used === null ? null : `${cost.used}${cost.unit === undefined || cost.unit === null ? "" : ` ${cost.unit}`}`
    case "unavailable":
      return null
  }
}
