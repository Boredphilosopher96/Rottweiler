import type { PickerItem } from "../components"
import type { PickerController } from "../picker-controller"
import type { CostSnapshot } from "../protocol"
import { formatSessionCost, formatTokenCount } from "../render"
import type { RottweilerState } from "../state"

interface UsageHost {
  readonly ui: { readonly state: RottweilerState; openBudgetPicker(): void }
  readonly pickerController: PickerController
}

type UsageRow = "budget" | "info"

const section = (id: string, label: string): PickerItem<UsageRow> =>
  ({ id: `usage.section.${id}`, label, description: "", sectionHeader: true, value: "info" })

/**
 * Usage: this session's tokens and charges, grouped by what they measure,
 * with the configured budget limits one Enter away.
 */
export function renderUsage(host: UsageHost): void {
  const cost = host.ui.state.cost
  const budget: PickerItem<UsageRow> = {
    id: "usage.budget",
    label: "Budget limits",
    hint: cost === null ? "" : cost.hard_cap_reached ? "limit reached" : limitSummary(cost),
    ...(cost?.hard_cap_reached === true ? { tone: "error" as const } : {}),
    description: "Set spend and token limits",
    detail: `${cost?.hard_cap_reached === true ? "A configured limit has been reached; new requests are refused until you raise it.\n\n" : ""}Session and daily spend caps, token caps, and the warning threshold.${cost === null ? "" : ` Today is ${cost.utc_day} UTC.`}`,
    primary: "open",
    value: "budget",
  }
  const open = (item: PickerItem<UsageRow>) => { if (item.value === "budget") host.ui.openBudgetPicker() }
  if (cost === null) {
    host.pickerController.show("USAGE   /usage", [section("limits", "Limits"), budget], open,
      { notice: { message: "reading usage…", tone: "muted" } })
    return
  }
  const usage = cost.session_usage
  const fact = (id: string, label: string, value: string, detail: string): PickerItem<UsageRow> =>
    ({ id: `usage.${id}`, label, hint: value, description: value, detail, primary: null, value: "info" })
  const items: PickerItem<UsageRow>[] = [
    section("session", "This session"),
    fact("total", "Session total", formatSessionCost(cost), `Everything this session has used so far, as priced by its providers.${cost.session_monetary_accounting_complete ? "" : "\n\nSome entries have no known price, so this total is incomplete."}`),
    fact("input", "Input tokens", formatTokenCount(usage.input_tokens), "Tokens sent to the model, including context re-sent each turn."),
    fact("output", "Output tokens", formatTokenCount(usage.output_tokens), "Tokens the model generated."),
    fact("reasoning", "Reasoning tokens", formatTokenCount(usage.reasoning_tokens), "Tokens the model spent thinking before answering."),
    fact("cache", "Cache read / write", `${formatTokenCount(usage.cache_read_tokens)} / ${formatTokenCount(usage.cache_write_tokens)}`,
      `Prompt-cache hits are cheaper than fresh input. Hit rate ${(cost.cache_hit_basis_points / 100).toFixed(1)}%.`),
    section("charges", "Charges"),
    fact("usd", "Known USD charges", `$${micros(cost.session_cost_micros_usd, 4)}${cost.session_monetary_accounting_complete ? "" : " · incomplete"}`,
      "Charges from providers that report USD prices."),
    fact("credits", "AI credits", micros(cost.session_ai_credit_micros, 3), "Credits consumed by providers that bill in credits."),
    fact("unavailable", "Requests without a price", cost.session_cost_unavailable_entries, "Requests whose price the provider did not report."),
    ...(Number(cost.session_subscription_quota_entries) > 0
      ? [fact("subscription", "Subscription entries", cost.session_subscription_quota_entries, "Requests covered by a subscription quota rather than per-token prices.")]
      : []),
    section("limits", "Limits"),
    budget,
  ]
  host.pickerController.show("USAGE   /usage", items, open, { selectedId: "usage.total" })
}

function micros(value: string, digits: number): string {
  return (Number(value) / 1_000_000).toFixed(digits)
}

function limitSummary(cost: CostSnapshot): string {
  return cost.session_cost_cap_micros_usd === null ? "no session cap" : `session cap $${micros(cost.session_cost_cap_micros_usd, 2)}`
}
