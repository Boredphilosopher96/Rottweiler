import { contextWarning } from "../state/context-usage"
import type { PickerItem } from "../components"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker } from "../projection-requests"
import { formatPercent, formatSessionCost, formatTokenCount } from "../render"
import type { RottweilerState } from "../state"

type Screen = "context" | "cost"
interface ContextHost {
  readonly ui: { readonly state: RottweilerState }
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly sessionId: string
}
export class ContextUiController {
  #selectedId: string | null = null
  #pending = false
  constructor(readonly host: ContextHost) {}
  open(screen: Screen): void {
    this.#selectedId = null
    this.host.pickerController.begin(screen)
    if (screen === "context") this.host.requests.command({ type: "list_commands" })
    this.host.requests.command({ type: screen === "context" ? "get_context" : "get_cost" })
    this.host.pickerController.refresh()
  }
  render(screen: Screen | "contextActions"): void {
    const state = this.host.ui.state
    if (screen === "contextActions") { this.#renderActions(); return }
    const snapshot = screen === "context" ? state.context : state.cost
    if (snapshot === null) {
      this.host.pickerController.show(screen === "context" ? "Context" : "Usage & cost", [{
        id: "refresh", label: "Waiting for the session snapshot", description: "Select to retry", value: null,
      }], () => this.open(screen))
      return
    }
    if (screen === "cost") { this.#renderCost(); return }
    const context = state.context!
    const warning = contextWarning(context)
    const items: PickerItem<string>[] = [{ id: "refresh", label: "Refresh context", description:
      context.context_window_known
        ? `${formatTokenCount(context.used_tokens)} / ${formatTokenCount(context.usable_tokens)} tokens · ${formatPercent(context.used_tokens, context.usable_tokens)} used · ${formatTokenCount(context.reserved_tokens)} reserved`
        : `${formatTokenCount(context.used_tokens)} tokens · context limit unknown`, value: "refresh" },
      ...(warning === null ? [] : [{ id: "warning", label: warning.split(" · ")[0]!, description: warning.split(" · ")[1]!, value: "warning", selectable: false }]),
      ...context.items.map(item => ({ id: `item:${item.item_id}`, label: item.label,
        description: `${formatTokenCount(item.estimated_tokens)} tokens · ${item.kind.replaceAll("_", " ")} · ${Object.entries(item.state).filter(([, active]) => active).map(([name]) => name).join(", ") || "in context"}`,
        value: item.item_id }))]
    this.host.pickerController.show("Context · select an item to pin or remove", items, item => {
      if (item.id === "refresh") { this.open("context"); return }
      this.#selectedId = item.value
      this.host.pickerController.begin("contextActions")
      this.host.pickerController.refresh()
    })
  }
  #renderActions(): void {
    const state = this.host.ui.state
    const item = state.context?.items.find(item => item.item_id === this.#selectedId)
    if (item === undefined) { this.open("context"); return }
    const unavailable = this.#pending ? "Applying change…" : state.replay.active ? "Return to the live session to change context"
      : state.availableActions.find(action => action.action === "mutate_context")?.unavailable_reason
        ?? (state.availableActions.some(action => action.action === "mutate_context") ? null : "Refresh context to check action availability")
    const reason = !item.item_id.startsWith("conversation:") ? "This context item is managed by the engine and cannot be pinned or removed" : unavailable ?? (state.compaction.active || Object.values(state.turns).some(turn => turn.status === "running") ? "Wait for the active response to finish" : null)
    const choices: PickerItem<"pin" | "evict" | "back">[] = [
      { id: "back", label: "Back to context", description: `${item.label} · ${formatTokenCount(item.estimated_tokens)} tokens`, value: "back" },
      { id: "pin", label: item.state.pinned ? "Already pinned" : "Pin item", description: reason ?? "Keep this item through compaction", value: "pin", selectable: reason === null && !item.state.pinned && !item.state.evicted },
      { id: "evict", label: item.state.evicted ? "Already removed" : "Remove from context", description: reason ?? "Preserve session history; exclude this item from future context", value: "evict", selectable: reason === null && !item.state.evicted },
    ]
    this.host.pickerController.show(item.label, choices, choice => {
      if (choice.value === "back") { this.open("context"); return }
      if (reason !== null || item.state.evicted || (choice.value === "pin" && item.state.pinned)) return
      void this.#change(choice.value, item.item_id)
    })
  }
  async #change(action: "pin" | "evict", itemId: string): Promise<void> {
    if (this.#pending) return
    this.#pending = true
    const sessionId = this.host.sessionId
    this.host.pickerController.refresh()
    try {
      using allocation = this.host.requests.allocate()
      const outcome = await this.host.requests.emit({ type: action === "pin" ? "pin_context" : "evict_context",
        meta: this.host.requests.meta(), session_id: sessionId, item_id: itemId }, allocation)
      if (this.host.sessionId !== sessionId || this.host.pickerController.kind !== "contextActions") return
      if (outcome?.type === "accepted") this.open("context")
      else this.host.pickerController.show("Context change not applied", [{ id: "back", label: "Back to context",
        description: outcome?.type === "rejected" ? outcome.error.message : "The engine connection is unavailable", value: null }], () => this.open("context"))
    } catch {
      if (this.host.sessionId === sessionId && this.host.pickerController.kind === "contextActions")
        this.host.pickerController.showStatus("Context", "Couldn't change context", "Refresh context and try again.")
    } finally { this.#pending = false }
  }
  #renderCost(): void {
    const cost = this.host.ui.state.cost!
    const usage = cost.session_usage
    const rows = [
      ["Session", formatSessionCost(cost)],
      ["Input tokens", formatTokenCount(usage.input_tokens)], ["Output tokens", formatTokenCount(usage.output_tokens)],
      ["Reasoning tokens", formatTokenCount(usage.reasoning_tokens)],
      ["Cache read / write", `${formatTokenCount(usage.cache_read_tokens)} / ${formatTokenCount(usage.cache_write_tokens)}`],
      ["Cache hit rate", `${(cost.cache_hit_basis_points / 100).toFixed(1)}%`],
      ["Known USD charges", `$${(Number(cost.session_cost_micros_usd) / 1_000_000).toFixed(4)}${cost.session_monetary_accounting_complete ? "" : " · incomplete accounting"}`],
      ["AI credits", (Number(cost.session_ai_credit_micros) / 1_000_000).toFixed(3)],
      ["Unavailable pricing entries", cost.session_cost_unavailable_entries],
      ["Non-USD entries", cost.session_non_usd_monetary_entries],
      ["Subscription entries", cost.session_subscription_quota_entries],
      ["Session USD cap", cost.session_cost_cap_micros_usd === null ? "Not set" : `$${Number(cost.session_cost_cap_micros_usd) / 1_000_000}`],
      ["Budget", cost.hard_cap_reached ? "Limit reached" : "Within configured limits"],
    ]
    this.host.pickerController.show("Usage & cost · current session", [{ id: "refresh", label: "Refresh usage", description: `Today: ${cost.utc_day} UTC`, value: "refresh" },
      ...rows.map(([label, description], index) => ({ id: String(index), label: label!, description: description!, value: "detail" }))], item => {
      if (item.id === "refresh") this.open("cost")
    })
  }
}
