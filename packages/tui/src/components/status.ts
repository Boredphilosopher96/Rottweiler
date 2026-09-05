import {
  TextRenderable,
  bg,
  bold,
  fg,
  t,
  type RenderContext
} from "@opentui/core"
import {
  formatStatusContext,
  formatStatusModel,
  formatStatusSessionCost,
  presentError
} from "../render"
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
    const waitingApproval = Object.values(state.tools).find(
      (tool) => tool.status === "awaiting_approval",
    )
    const permissionMode = permissionRuntimeMode(state.permissions)
    const hasSessionActivity =
      state.replay.active ||
      state.hasActivity ||
      state.streamingTail !== null ||
      Object.keys(state.tools).length > 0
    const context =
      state.context === null
        ? (hasSessionActivity ? "ctx —" : null)
        : headlineContext(formatStatusContext(state.context))
    const pluginStatus = Object.entries(state.pluginStatuses).at(-1)
    const statusModel = state.model === null
      ? null
      : formatStatusModel(state.model, state.provider, state.models)
    const statusProvider = statusModel?.includes("/") === true
      ? statusModel.slice(0, statusModel.indexOf("/"))
      : state.provider
    const mode = state.replay.active ? "REPLAY" : (state.mode ?? "—").toUpperCase()
    const modeColor = state.replay.active ? this.#theme.info : this.#theme.primary
    const modePill = bg(modeColor)(fg(this.#theme.background)(` ${mode} `))
    const model = statusModel === null
      ? `model not selected${this.#modelPickerKeycap === null ? "" : ` · ${this.#modelPickerKeycap}`}`
      : compactStatusModel(statusModel)
    const approval = waitingApproval === undefined
      ? ""
      : `  approval · ${toolDisplayName(waitingApproval.name)}`
    const cost = state.cost === null && !hasSessionActivity
      ? ""
      : `  ${formatStatusSessionCost(state.cost, statusProvider, state.context?.used_tokens ?? null)}`
    const branch = this.#branch === null && !hasSessionActivity ? "" : `  ${this.#branch ?? "—"}`
    const changedCount = state.workspaceStatus?.changedPaths.length ?? 0
    const changed = changedCount === 0 ? "" : `  ${changedCount} changed`
    const runningAgents = Object.values(state.subagents)
      .filter((subagent) => subagent.status === "running").length
    const extension = pluginStatus === undefined ? "" : `  Extension · ${humanLabel(pluginStatus[1])}`
    const contextLabel = context === null ? "" : context.replace(/^(ctx)\s*/, "")
    const agentLabel = runningAgents === 1 ? " agent" : " agents"
    this.content = t`${bold(modePill)}${permissionMode === null ? "" : fg(this.#theme.textMuted)(`  ${permissionMode}`)}  ${fg(this.#theme.textMuted)(model)}${approval === "" ? "" : fg(this.#theme.warning)(approval)}${contextLabel === "" ? "" : fg(this.#theme.border)("    ctx ")}${contextLabel === "" ? "" : fg(this.#theme.text)(contextLabel)}${fg(this.#theme.text)(cost)}${branch === "" ? "" : fg(this.#theme.secondary)(branch)}${changed === "" ? "" : fg(this.#theme.warning)(changed)}${runningAgents === 0 ? "" : fg(this.#theme.info)(`    ${runningAgents}`)}${runningAgents === 0 ? "" : fg(this.#theme.textMuted)(agentLabel)}${extension === "" ? "" : fg(this.#theme.textMuted)(extension)}`
  }
}

function compactStatusModel(model: string): string {
  const separator = model.indexOf("/")
  return separator < 0 ? model : model.slice(separator + 1)
}

function headlineContext(context: string): string {
  const percent = /\(([^)]+)\)$/.exec(context)?.[1]
  return percent === undefined ? context : `ctx ${percent}`
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
    if (latestError !== undefined) {
      const presentation = presentError(latestError)
      this.visible = true
      this.fg = this.#theme[presentation.severity]
      this.content = presentation.text
    } else if (latestBudget !== undefined && latestBudget.level === "hard_cap") {
      this.visible = true
      this.fg = this.#theme.error
      this.content = `Budget limit reached · ${budgetScopeLabel(latestBudget.scope)} · ${formatBudgetAmount(latestBudget.current, latestBudget.unit)} of ${formatBudgetAmount(latestBudget.limit, latestBudget.unit)}`
    } else if (waitingApproval !== undefined) {
      this.visible = true
      this.fg = this.#theme.warning
      this.content = `Waiting for approval · ${toolDisplayName(waitingApproval.name)}`
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
