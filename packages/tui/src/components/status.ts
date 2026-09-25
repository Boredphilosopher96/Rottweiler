import { TextRenderable } from "./text"
import { contextWarning, statusContext } from "../state/context-usage"
import {
  bg,
  bold,
  fg,
  t,
  type RenderContext
} from "@opentui/core"
import {
  contextPercent,
  formatKnownSessionCost,
  formatTokenCount,
  modelDisplayLabel,
  presentError
} from "../render"
import { permissionModeLabel } from "../ui-presentation"
import type { RottweilerState } from "../state"
import type { RottweilerTheme } from "../theme"
import { humanLabel, permissionRuntimeMode, toolDisplayName } from "./panel-labels"

export class StatusLineRenderable extends TextRenderable {
  #branch: string | null = null
  readonly #modelPickerKeycap: string | null
  readonly #theme: RottweilerTheme

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    options: { readonly modelPickerKeycap?: string | null } = {},
  ) {
    super(ctx, {
      id: "status-line",
      width: "auto",
      height: 1,
      content: "",
      fg: theme.textMuted,
      bg: theme.backgroundPanel,
      marginLeft: 1,
      marginRight: 1,
      truncate: true,
    })
    this.#modelPickerKeycap = options.modelPickerKeycap ?? null
    this.#theme = theme
  }

  setBranch(branch: string | null): void {
    this.#branch = branch
  }

  setKeybindingMode(
    _mode: "normal" | "insert" | null,
    _target: "composer" | "transcript" | "picker" | "interaction" | "review" | null,
  ): void {
    // Input-mode chrome belongs next to the composer. Keep this method so the
    // app's focus state does not leak into the session identity row.
  }

  update(state: RottweilerState): void {
    const theme = this.#theme
    const waitingApproval = Object.values(state.tools).find(
      (tool) => tool.status === "awaiting_approval",
    )
    const permissionMode = permissionRuntimeMode(state.permissions)
    const hasSessionActivity =
      state.replay.active ||
      state.hasActivity ||
      state.streamingTail !== null ||
      Object.keys(state.tools).length > 0
    const mode = state.replay.active ? "REPLAY" : (state.mode ?? "execute").toUpperCase()
    const modeColor = state.replay.active ? theme.info : theme.primary
    const modePill = bg(modeColor)(fg(theme.background)(` ${mode} `))
    const keycap = this.#modelPickerKeycap === null ? "" : ` · ${this.#modelPickerKeycap}`
    const modelName = modelDisplayLabel(state.model, state.models)
    // Replay is read-only and never loads the model catalog: show the recorded
    // model when the history names one, and no model call to action.
    const model = state.replay.active
      ? modelName ?? state.model ?? ""
      : modelName ?? `${missingModelLabel(state)}${keycap}`
    const usage = statusContext(state)
    const percent = usage === null ? null : contextPercent(usage)
    const contextLabel = usage === null
      ? (hasSessionActivity ? "ctx —" : "")
      : percent === null
        ? `ctx ${formatTokenCount(usage.used_tokens)}`
        : `ctx ${percent}%`
    const contextColor = percent === null ? theme.textMuted
      : percent >= 85 ? theme.error : percent >= 70 ? theme.warning : theme.text
    const cost = formatKnownSessionCost(state.cost)
    const branch = this.#branch
    const changedCount = state.workspaceStatus?.changes.length ?? 0
    const runningAgents = Object.values(state.subagents)
      .filter((subagent) => subagent.status === "running").length
    const queuedControls = state.queuedControls.length === 0
      ? state.lastControlSettlement !== null && state.lastControlSettlement.outcome !== "applied" ? `queued control ${state.lastControlSettlement.outcome} · open queue` : ""
      : `${state.queuedControls.length} queued`
    const pluginStatus = Object.entries(state.pluginStatuses).at(-1)
    const gap = "  "
    const segment = (text: string, color: string) => text === "" ? "" : fg(color)(`${gap}${text}`)
    this.content = t`${bold(modePill)}${segment(permissionMode === null ? "" : `approvals ${permissionModeLabel(permissionMode)}`, permissionMode === "yolo" ? theme.warning : theme.textMuted)}${segment(model, modelName === null && !state.replay.active ? theme.warning : theme.text)}${segment(contextLabel, contextColor)}${segment(cost ?? "", theme.text)}${segment(branch ?? "", theme.secondary)}${segment(changedCount === 0 ? "" : `${changedCount} changed`, theme.warning)}${segment(waitingApproval === undefined ? "" : `approval · ${toolDisplayName(waitingApproval.name)}`, theme.warning)}${segment(runningAgents === 0 ? "" : `${runningAgents} agent${runningAgents === 1 ? "" : "s"} running`, theme.info)}${segment(queuedControls, theme.textMuted)}${segment(pluginStatus === undefined ? "" : humanLabel(pluginStatus[1]), theme.textMuted)}`
  }
}

/** One call to action while no usable model is selected. */
function missingModelLabel(state: RottweilerState): string {
  if (!state.modelCatalogLoaded || state.modelCatalogCached) return "loading models"
  return state.providers.some(provider => provider.configured && provider.authenticated)
    ? "choose a model"
    : "connect a provider"
}

export class StateBannerRenderable extends TextRenderable {
  #theme: RottweilerTheme

  constructor(ctx: RenderContext, theme: RottweilerTheme) {
    super(ctx, {
      id: "state-banner",
      width: "100%",
      height: 1,
      content: "",
      fg: theme.info,
      bg: theme.backgroundElement,
      visible: false,
      truncate: true,
    })
    this.#theme = theme
  }

  update(state: RottweilerState): void {
    const latestBudget = state.budgets.at(-1)
    const latestError = state.errors.at(-1)
    const latestPluginNotification = state.pluginNotifications.at(-1)
    const waitingApproval = Object.values(state.tools).find(
      (tool) => tool.status === "awaiting_approval",
    )
    // A background projection failure remains inspectable, but must not obscure
    // the live decision currently occupying the interaction panel.
    const deferredQueryFailure = waitingApproval !== undefined && latestError?.code === "host_query_failure"
    if (latestError !== undefined && !deferredQueryFailure) {
      const presentation = presentError(latestError)
      this.visible = true
      this.fg = this.#theme[presentation.severity]
      this.content = presentation.text
    } else if (latestBudget !== undefined && latestBudget.level === "hard_cap") {
      this.visible = true
      this.fg = this.#theme.error
      this.content = `Budget limit reached · ${budgetScopeLabel(latestBudget.scope)} · ${formatBudgetAmount(latestBudget.current, latestBudget.unit)} of ${formatBudgetAmount(latestBudget.limit, latestBudget.unit)}`
    } else if (state.replay.active) {
      this.visible = true
      this.fg = this.#theme.info
      const progress =
        state.replay.completedThrough === null
          ? state.historyReady?.sessionId === state.replay.sessionId
            ? "history available" : "loading history…"
          : `complete through event ${state.replay.completedThrough}`
      this.content = `Replay · ${state.replay.sessionId ?? "historical session"} · read-only · ${progress}`
    } else if (state.compaction.active) {
      this.visible = true
      this.fg = this.#theme.info
      this.content = `Compacting context · ${compactionReasonLabel(state.compaction.reason)} · UI remains responsive`
    } else if (state.connection.phase !== "connected" && state.connection.phase !== "idle") {
      this.visible = true
      this.fg = this.#theme.warning
      this.content = state.connection.gap === null
        ? connectionMessage(state.connection.phase)
        : "Restoring missed updates…"
    } else if (contextWarning(statusContext(state)) !== null) {
      this.visible = true
      this.fg = this.#theme.warning
      this.content = contextWarning(statusContext(state))!
    } else if (latestPluginNotification !== undefined) {
      this.visible = true
      this.fg = this.#theme.info
      this.content = `${latestPluginNotification.title} · ${latestPluginNotification.message}`
    } else {
      this.visible = false
      this.content = ""
    }
  }
}

function connectionMessage(phase: RottweilerState["connection"]["phase"]): string {
  switch (phase) {
    case "connecting": return "Connecting to the engine…"
    case "reconnecting": return "Reconnecting to the engine…"
    case "replaying": return "Restoring the session…"
    case "disconnected": return "Connection lost · retrying…"
    case "closed": return "Engine stopped"
    case "connected": return "Connected"
    case "idle": return ""
  }
}

function budgetScopeLabel(scope: string): string {
  switch (scope) {
    case "session": return "This session"
    case "daily": return "Today"
    case "trailing_minute": return "Recent usage"
    default: return "Usage"
  }
}

function formatBudgetAmount(value: string, unit: string): string {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) return "unknown"
  if (unit === "tokens") return `${BigInt(value).toLocaleString()} tokens`
  const micros = BigInt(value)
  const whole = micros / 1_000_000n
  const fraction = (micros % 1_000_000n).toString().padStart(6, "0").replace(/0+$/, "")
  const amount = fraction.length === 0 ? `${whole}` : `${whole}.${fraction}`
  return unit === "micros_usd" ? `$${amount}` : `${amount} AI credits`
}

function compactionReasonLabel(reason: string | null): string {
  if (reason === null || reason === "manual") return "Requested"
  if (reason === "context_overflow") return "Making room for more context"
  return "Keeping the conversation responsive"
}
