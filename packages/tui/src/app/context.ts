import { StyledText, fg } from "@opentui/core"
import type { PickerItem, PickerKey } from "../components"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker } from "../projection-requests"
import type { ContextSnapshot } from "../protocol"
import { formatTokenCount } from "../render"
import type { RottweilerState } from "../state"
import type { RottweilerTheme } from "../theme"
import { renderUsage } from "./usage"

export type ContextScreen = "context" | "contextItems" | "cost"
type ContextItemSnapshot = ContextSnapshot["items"][number]
type ContextItemKind = ContextItemSnapshot["kind"]

interface ContextHost {
  readonly ui: { readonly state: RottweilerState; openBudgetPicker(): void; closePicker(): void }
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly sessionId: string
  readonly theme: RottweilerTheme
}

type CategoryId = "system" | "tools" | "conversation" | "pinned"

interface Category {
  readonly id: CategoryId
  readonly label: string
  readonly description: string
  readonly kinds: readonly ContextItemKind[]
}

/** Prompt order: stable instructions first, then tools, then the evolving conversation. */
const CATEGORIES: readonly Category[] = [
  { id: "system", label: "System & instructions", kinds: ["system", "project_instructions"],
    description: "The system prompt and AGENTS.md-style project instructions. Managed by the engine." },
  { id: "tools", label: "Tools", kinds: ["tool_definitions"],
    description: "Tool and skill definitions the model can call. Managed by the engine." },
  { id: "pinned", label: "Pinned", kinds: ["pinned"],
    description: "Conversation items you pinned. They survive compaction verbatim." },
  { id: "conversation", label: "Conversation", kinds: ["conversation", "tool_result", "queued_message"],
    description: "Messages, tool results, and queued messages. Pin to keep an item through compaction; remove to exclude it from future requests." },
]

type Row = { readonly kind: "category"; readonly id: CategoryId } | { readonly kind: "free" } | { readonly kind: "item"; readonly item: ContextItemSnapshot }

/**
 * Context: what the next request sends, as a usage meter and categories with
 * totals. Opening a category lists its items; conversation items are pinned
 * or removed directly. The snapshot refreshes itself as the session advances.
 */
export class ContextUiController {
  #category: CategoryId | null = null
  #pending = false
  #notice: string | null = null
  #requestedThrough: string | null | undefined = undefined
  constructor(readonly host: ContextHost) {}

  open(screen: "context" | "cost"): void {
    this.#category = null
    this.#notice = null
    this.host.pickerController.begin(screen)
    this.#request(screen)
    this.host.pickerController.refresh()
  }

  render(screen: ContextScreen): void {
    if (screen === "cost") {
      const through = this.host.ui.state.contextUsage?.through
      if (this.host.ui.state.cost !== null && through !== this.#requestedThrough) this.#request("cost")
      renderUsage(this.host)
      return
    }
    const context = this.host.ui.state.context
    if (context === null) {
      this.host.pickerController.showLoading("CONTEXT   /context", "Reading the session context")
      return
    }
    this.#refreshWhenStale(context)
    if (screen === "contextItems" && this.#category !== null) this.#renderItems(context, this.#category)
    else this.#renderCategories(context)
  }

  #request(screen: "context" | "cost"): void {
    this.#requestedThrough = this.host.ui.state.contextUsage?.through
    if (screen === "context") {
      // Availability decides whether pin, remove, and compact apply right now.
      this.host.requests.command({ type: "list_commands" })
    }
    this.host.requests.command({ type: screen === "context" ? "get_context" : "get_cost" })
  }

  /** Live usage advances with each turn; re-read the inspector snapshot once per change. */
  #refreshWhenStale(context: ContextSnapshot): void {
    const live = this.host.ui.state.contextUsage
    if (live === null || live.through === context.through || live.through === this.#requestedThrough) return
    this.#request("context")
  }

  #renderCategories(context: ContextSnapshot): void {
    const used = decimal(context.used_tokens)
    const usable = decimal(context.usable_tokens)
    const items: PickerItem<Row>[] = CATEGORIES.flatMap(category => {
      const members = context.items.filter(item => category.kinds.includes(item.kind) && !item.state.evicted)
      if (members.length === 0 && category.id === "pinned") return []
      const tokens = members.reduce((sum, item) => sum + decimal(item.estimated_tokens), 0)
      const largest = [...members].sort((left, right) => decimal(right.estimated_tokens) - decimal(left.estimated_tokens)).slice(0, 6)
      return [{
        id: `context.category.${category.id}`,
        label: category.label,
        hint: `${formatTokens(tokens)}${used > 0 ? ` · ${share(tokens, used)}` : ""}`,
        description: `${members.length} ${members.length === 1 ? "item" : "items"} · ${formatTokens(tokens)} tokens`,
        detail: [
          category.description,
          "",
          `${members.length} ${members.length === 1 ? "item" : "items"} · ${formatTokens(tokens)} tokens`,
          ...(largest.length === 0 ? [] : ["", "Largest", ...largest.map(item => `  ${formatTokenCount(item.estimated_tokens).padStart(6)}  ${itemLabel(item)}`)]),
        ].join("\n"),
        primary: members.length === 0 ? null : "open",
        value: { kind: "category", id: category.id } as const,
      }]
    })
    if (context.context_window_known) {
      const free = Math.max(0, usable - used)
      items.push({
        id: "context.free",
        label: "Free",
        hint: `${formatTokens(free)} · ${share(free, usable)}`,
        tone: "muted",
        description: `${formatTokens(free)} tokens available before the window is full`,
        detail: [
          `${formatTokens(free)} of ${formatTokens(usable)} usable tokens are free.`,
          `${formatTokenCount(context.reserved_tokens)} more are reserved for the model's reply and are not counted as usable.`,
          "",
          "Rottweiler compacts older conversation automatically as the window fills; ctrl+k compacts now.",
        ].join("\n"),
        primary: null,
        value: { kind: "free" },
      })
    }
    const evicted = context.items.filter(item => item.state.evicted).length
    this.host.pickerController.show("CONTEXT", items, item => {
      if (item.value.kind !== "category") return
      this.#category = item.value.id
      this.#notice = null
      this.host.pickerController.kind = "contextItems"
      this.host.pickerController.refresh()
    }, {
      heading: this.#heading("CONTEXT", context),
      keys: [this.#compactKey()],
      notice: this.#noticeFor(evicted > 0 ? `${evicted} removed ${evicted === 1 ? "item" : "items"} excluded` : null),
    })
  }

  #renderItems(context: ContextSnapshot, categoryId: CategoryId): void {
    const category = CATEGORIES.find(candidate => candidate.id === categoryId)!
    const members = context.items.filter(item => category.kinds.includes(item.kind))
    const blocked = this.#mutationBlocked()
    const items: PickerItem<Row>[] = members.map(item => {
      const mutable = item.item_id.startsWith("conversation:")
      const flags = [
        item.state.pinned ? "pinned" : null,
        item.state.evicted ? "removed" : null,
        item.state.summarized ? "summarized" : null,
        item.state.pruned ? "pruned" : null,
      ].filter((flag): flag is string => flag !== null)
      return {
        id: `context.item.${item.item_id}`,
        label: itemLabel(item),
        hint: [...flags, formatTokenCount(item.estimated_tokens)].join(" · "),
        ...(item.state.pinned ? { marker: "◆" } : item.state.evicted ? { marker: "−" } : {}),
        ...(item.state.evicted ? { tone: "muted" as const } : {}),
        description: `${formatTokenCount(item.estimated_tokens)} tokens · ${item.kind.replaceAll("_", " ")}`,
        detail: [
          `${formatTokenCount(item.estimated_tokens)} tokens · ${item.kind.replaceAll("_", " ")}${flags.length === 0 ? "" : ` · ${flags.join(", ")}`}`,
          `source  ${item.source}`,
          "",
          !mutable ? "Managed by the engine; it cannot be pinned or removed."
            : blocked ?? (item.state.evicted ? "Removed: kept in session history, excluded from future requests."
              : item.state.pinned ? "Pinned: kept verbatim through compaction."
                : "Enter pins this item through compaction; ctrl+d removes it from future requests."),
        ].join("\n"),
        primary: mutable && blocked === null && !item.state.pinned && !item.state.evicted ? "pin" : null,
        value: { kind: "item", item } as const,
      }
    })
    const removable = (row: PickerItem<Row> | null) => row?.value.kind === "item" && blocked === null
      && row.value.item.item_id.startsWith("conversation:") && !row.value.item.state.evicted
    const back = () => {
      this.#category = null
      this.#notice = null
      this.host.pickerController.kind = "context"
      this.host.pickerController.refresh()
    }
    this.host.pickerController.show(`CONTEXT › ${category.label}`, items, row => {
      if (row.value.kind === "item") void this.#change("pin", row.value.item.item_id)
    }, {
      heading: this.#heading(`CONTEXT › ${category.label}`, context),
      back,
      emptyCopy: `No ${category.label.toLocaleLowerCase()} items in context`,
      keys: [
        { stroke: "ctrl+d", label: "remove", available: removable,
          run: row => { if (row?.value.kind === "item") void this.#change("evict", row.value.item.item_id) } },
        this.#compactKey(),
      ],
      notice: this.#noticeFor(null),
    })
  }

  #compactKey(): PickerKey<Row> {
    const state = this.host.ui.state
    const compact = state.availableActions.find(action => action.action === "compact")
    return {
      stroke: "ctrl+k",
      label: "compact",
      available: () => !state.replay.active && !state.compaction.active
        && (compact === undefined || compact.unavailable_reason === null),
      run: () => {
        this.host.requests.dispatch({ type: "compact", meta: this.host.requests.meta(), session_id: this.host.sessionId, instructions: null })
        this.host.ui.closePicker()
      },
    }
  }

  #noticeFor(info: string | null): { readonly message: string; readonly tone: "muted" | "warning" | "error" } | null {
    if (this.#pending) return { message: "Applying change…", tone: "muted" }
    if (this.#notice !== null) return { message: this.#notice, tone: "error" }
    const context = this.host.ui.state.context
    if (context?.context_window_known === true && decimal(context.usable_tokens) > 0) {
      const percent = decimal(context.used_tokens) / decimal(context.usable_tokens) * 100
      if (percent >= 85) return { message: "near limit", tone: "error" }
      if (percent >= 70) return { message: "filling up", tone: "warning" }
    }
    return info === null ? null : { message: info, tone: "muted" }
  }

  #mutationBlocked(): string | null {
    const state = this.host.ui.state
    if (this.#pending) return "Applying the previous change…"
    if (state.replay.active) return "Return to the live session to change context."
    const availability = state.availableActions.find(action => action.action === "mutate_context")
    if (availability?.unavailable_reason != null) return availability.unavailable_reason
    if (availability === undefined) return "Checking whether context can change right now…"
    if (state.compaction.active || Object.values(state.turns).some(turn => turn.status === "running")) {
      return "Wait for the active response to finish."
    }
    return null
  }

  #heading(title: string, context: ContextSnapshot): StyledText {
    const theme = this.host.theme
    const chunks: StyledText["chunks"] = [fg(theme.text)(`${title}   `)]
    if (!context.context_window_known || decimal(context.usable_tokens) <= 0) {
      chunks.push(fg(theme.textMuted)(`${formatTokenCount(context.used_tokens)} used · context limit unknown`))
      return new StyledText(chunks)
    }
    const percent = decimal(context.used_tokens) / decimal(context.usable_tokens) * 100
    const color = percent >= 85 ? theme.error : percent >= 70 ? theme.warning : theme.primary
    const width = 12
    const filled = Math.min(width, Math.round(percent / 100 * width))
    chunks.push(fg(color)("█".repeat(filled)), fg(theme.borderSubtle)("░".repeat(width - filled)))
    chunks.push(fg(color)(` ${Math.min(999, Math.round(percent))}%`))
    chunks.push(fg(theme.textMuted)(` · ${formatTokenCount(context.used_tokens)}/${formatTokenCount(context.usable_tokens)}`))
    return new StyledText(chunks)
  }

  async #change(action: "pin" | "evict", itemId: string): Promise<void> {
    if (this.#pending) return
    this.#pending = true
    this.#notice = null
    const sessionId = this.host.sessionId
    this.host.pickerController.refresh()
    try {
      using allocation = this.host.requests.allocate()
      const outcome = await this.host.requests.emit({ type: action === "pin" ? "pin_context" : "evict_context",
        meta: this.host.requests.meta(), session_id: sessionId, item_id: itemId }, allocation)
      if (this.host.sessionId !== sessionId) return
      if (outcome?.type === "accepted") this.#request("context")
      else this.#notice = outcome?.type === "rejected" ? outcome.error.message : "The engine connection is unavailable."
    } catch {
      if (this.host.sessionId === sessionId) this.#notice = "Couldn't change context; try again."
    } finally {
      this.#pending = false
      if (this.host.pickerController.kind === "contextItems") this.host.pickerController.refresh()
    }
  }
}

function itemLabel(item: ContextItemSnapshot): string {
  if (item.kind === "tool_definitions") return item.label.replace(/^tool:/u, "")
  const turn = /^(User|Assistant) turn (\d+)$/u.exec(item.label)
  if (turn !== null) return `${turn[1] === "User" ? "You" : "Assistant"} · message ${turn[2]}`
  return item.label
}

function decimal(value: string): number {
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : 0
}

function formatTokens(value: number): string {
  return formatTokenCount(String(Math.round(value)))
}

function share(part: number, whole: number): string {
  return whole <= 0 ? "—%" : `${Math.round(part / whole * 100)}%`
}
