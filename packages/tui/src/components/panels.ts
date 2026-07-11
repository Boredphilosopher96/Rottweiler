import {
  BoxRenderable,
  DiffRenderable,
  SelectRenderable,
  SelectRenderableEvents,
  TextRenderable,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"

import { formatPercent, formatSessionCost } from "../render"
import type {
  ApprovalDecision,
  ContextSnapshot,
  PlanArtifact,
  PlanDecision,
  Question,
} from "../protocol"
import type { QuestionProjection, RottweilerState, ToolProjection } from "../state"
import type { RottweilerTheme } from "../theme"

export interface InteractionCallbacks {
  readonly onApproval: (tool: ToolProjection, decision: ApprovalDecision) => void
  readonly onAnswer: (question: QuestionProjection, values: readonly string[]) => void
  readonly onPlanReview: (decision: PlanDecision) => void
}

export type ReviewFileDecision = "accept" | "revert"

export interface ReviewPanelCallbacks {
  readonly onDecision: (
    file: NonNullable<RottweilerState["review"]>["files"][number],
    decision: ReviewFileDecision,
  ) => void
  readonly onClose: () => void
}

/** Retained cumulative session review with exact per-file decisions. */
export class ReviewPanelRenderable extends BoxRenderable {
  readonly summary: TextRenderable
  readonly files: SelectRenderable
  readonly hint: TextRenderable
  readonly diff: DiffRenderable
  #review: RottweilerState["review"] = null
  #callbacks: ReviewPanelCallbacks
  #pendingPaths = new Set<string>()
  #shellActive = false

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    callbacks: ReviewPanelCallbacks,
    treeSitterClient?: TreeSitterClient,
  ) {
    super(ctx, {
      id: "session-review",
      width: "100%",
      height: 17,
      flexShrink: 0,
      flexDirection: "column",
      border: true,
      borderStyle: "rounded",
      borderColor: theme.info,
      backgroundColor: theme.panel,
      paddingX: 1,
      visible: false,
      zIndex: 9,
    })
    this.#callbacks = callbacks
    this.summary = new TextRenderable(ctx, {
      content: "",
      fg: theme.foreground,
      height: 1,
      truncate: true,
    })
    this.diff = new DiffRenderable(ctx, {
      id: "session-review-diff",
      width: "100%",
      height: 8,
      diff: "",
      ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
      syntaxStyle,
      view: "unified",
      wrapMode: "none",
      showLineNumbers: true,
      addedBg: theme.added,
      removedBg: theme.removed,
      contextBg: theme.panel,
    })
    this.files = new SelectRenderable(ctx, {
      id: "session-review-files",
      width: "100%",
      height: 5,
      options: [],
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      selectedBackgroundColor: theme.selection,
      selectedTextColor: theme.accentStrong,
      descriptionColor: theme.muted,
      showScrollIndicator: true,
    })
    this.hint = new TextRenderable(ctx, {
      content: "A accept · R revert",
      fg: theme.muted,
      height: 1,
    })
    this.files.on(SelectRenderableEvents.SELECTION_CHANGED, () => this.#showSelected())
    this.files.onKeyDown = (key) => {
      if (key.name === "escape" || key.name === "esc") {
        key.preventDefault()
        this.#callbacks.onClose()
        return
      }
      if (key.name !== "a" && key.name !== "r") return
      key.preventDefault()
      const file = this.#review?.files[this.files.getSelectedIndex()]
      if (file === undefined) return
      if (this.#shellActive || this.#pendingPaths.has(file.path)) return
      const decision: ReviewFileDecision = key.name === "a" ? "accept" : "revert"
      if (decision === "revert" && file.unrestorableReason !== null) {
        return
      }
      this.#callbacks.onDecision(file, decision)
    }
    this.add(this.summary)
    this.add(this.diff)
    this.add(this.files)
    this.add(this.hint)
  }

  update(state: RottweilerState): void {
    const review = state.replay.active ? null : state.review
    this.#review = review
    this.#shellActive = state.shell.active
    if (review === null) {
      this.visible = false
      return
    }
    const selectedPath = review.files[this.files.getSelectedIndex()]?.path
    const pending = review.files.filter((file) => file.status === "pending").length
    const accepted = review.files.filter((file) => file.status === "accepted").length
    const reverted = review.files.filter((file) => file.status === "reverted").length
    this.title = ` Session review · ${review.files.length} files `
    this.summary.content = state.shell.active
      ? "Foreground shell active · review decisions disabled"
      : `${pending} pending · ${accepted} accepted · ${reverted} reverted`
    this.files.options = review.files.map((file) => ({
      name: `${reviewGlyph(file.status)} ${file.path}`,
      description:
        (this.#pendingPaths.has(file.path)
          ? "decision pending"
          : file.unrestorableReason ??
            (file.truncated ? "diff truncated · checkpoint revert available" : file.status)),
      value: file.path,
    }))
    const nextIndex = Math.max(
      0,
      review.files.findIndex((file) => file.path === selectedPath),
    )
    this.files.setSelectedIndex(nextIndex)
    this.visible = true
    this.#showSelected()
    this.files.focus()
  }

  setDecisionPending(path: string, pending: boolean): void {
    if (pending) this.#pendingPaths.add(path)
    else this.#pendingPaths.delete(path)
    this.#showSelected()
    const index = this.#review?.files.findIndex((file) => file.path === path) ?? -1
    if (index >= 0) {
      const option = this.files.options[index]
      if (option !== undefined) {
        option.description = pending ? "decision pending" : this.#fileDescription(index)
        this.files.options = [...this.files.options]
      }
    }
  }

  #showSelected(): void {
    const file = this.#review?.files[this.files.getSelectedIndex()]
    this.diff.diff = file?.unifiedDiff ?? ""
    this.diff.filetype = file === undefined ? undefined : extension(file.path)
    const revertUnavailable = file !== undefined && file.unrestorableReason !== null
    this.hint.content =
      file === undefined
        ? "No files changed in this session"
        : this.#shellActive
          ? "Exit the foreground shell before reviewing files"
          : this.#pendingPaths.has(file.path)
            ? "Decision pending…"
            : `A accept · ${revertUnavailable ? "R revert unavailable" : "R revert"}`
  }

  #fileDescription(index: number): string {
    const file = this.#review?.files[index]
    if (file === undefined) return ""
    return (
      file.unrestorableReason ??
      (file.truncated ? "diff truncated · checkpoint revert available" : file.status)
    )
  }
}

function reviewGlyph(status: "pending" | "accepted" | "reverted"): string {
  switch (status) {
    case "pending":
      return "◆"
    case "accepted":
      return "✓"
    case "reverted":
      return "↶"
  }
}

export class InteractionPanelRenderable extends BoxRenderable {
  readonly prompt: TextRenderable
  readonly select: SelectRenderable
  #diff: DiffRenderable | null = null
  #activeTool: ToolProjection | null = null
  #activeQuestion: QuestionProjection | null = null
  #activePlan: PlanArtifact | null = null
  #callbacks: InteractionCallbacks
  #syntaxStyle: SyntaxStyle
  #theme: RottweilerTheme
  #treeSitterClient: TreeSitterClient | undefined

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    callbacks: InteractionCallbacks,
    treeSitterClient?: TreeSitterClient,
  ) {
    super(ctx, {
      id: "interaction-panel",
      width: "100%",
      maxHeight: 18,
      flexDirection: "column",
      border: true,
      borderStyle: "rounded",
      borderColor: theme.warning,
      backgroundColor: theme.panelRaised,
      padding: 1,
      visible: false,
      zIndex: 10,
    })
    this.#theme = theme
    this.#syntaxStyle = syntaxStyle
    this.#callbacks = callbacks
    this.#treeSitterClient = treeSitterClient
    this.prompt = new TextRenderable(ctx, {
      content: "",
      fg: theme.foreground,
      wrapMode: "word",
      minHeight: 1,
    })
    this.select = new SelectRenderable(ctx, {
      width: "100%",
      minHeight: 3,
      options: [],
      backgroundColor: theme.panelRaised,
      textColor: theme.foreground,
      selectedBackgroundColor: theme.selection,
      selectedTextColor: theme.accentStrong,
      descriptionColor: theme.muted,
      wrapSelection: true,
    })
    this.select.on(SelectRenderableEvents.ITEM_SELECTED, (index: number) =>
      this.#selected(index),
    )
    this.add(this.prompt)
    this.add(this.select)
  }

  update(state: RottweilerState): void {
    if (state.replay.active) {
      this.#activeTool = null
      this.#activeQuestion = null
      this.#activePlan = null
      this.#removeDiff()
      this.visible = false
      return
    }
    const tool = Object.values(state.tools).find((candidate) => candidate.status === "awaiting_approval")
    const question = Object.values(state.questions).find((candidate) => !candidate.answered)
    const turnRunning = Object.values(state.turns).some((turn) => turn.status === "running")
    if (state.pendingPlan !== null && !turnRunning) {
      this.#showPlan(state.pendingPlan)
      return
    }
    if (tool !== undefined) {
      this.#showTool(tool)
      return
    }
    if (question !== undefined) {
      this.#showQuestion(question)
      return
    }
    this.#activeTool = null
    this.#activeQuestion = null
    this.#activePlan = null
    this.#removeDiff()
    this.visible = false
  }

  #showTool(tool: ToolProjection): void {
    this.#activeTool = tool
    this.#activeQuestion = null
    this.#activePlan = null
    this.visible = true
    const bash = bashApproval(tool)
    this.title = bash?.unsandboxed === true ? " UNSANDBOXED approval required " : " Permission required "
    const diff = readUnifiedDiff(tool.diff)
    const truncated = diff?.truncated === true
    const command = bash === null ? "" : `\n$ ${bash.command}`
    this.prompt.content = `${tool.name} requests ${tool.capabilities.join(", ") || "permission"}${command}\n${
      truncated
        ? "Diff exceeds the review limit. Approval is disabled until the complete change can be reviewed."
        : (tool.rationale ?? "Review this action.")
    }`
    this.select.options = truncated
      ? [{ name: "Deny", description: "A truncated change cannot be approved", value: "deny" }]
      : [
          { name: "Allow once", description: "Run only this invocation", value: "allow_once" },
          { name: "Allow session", description: "Remember for this session", value: "allow_session" },
          { name: "Allow project", description: "Remember this exact invocation in this project", value: "allow_project" },
          { name: "Deny", description: "Do not run the tool", value: "deny" },
        ]
    this.select.setSelectedIndex(0)
    if (diff !== null) {
      if (this.#diff === null) {
        const filetype = extension(diff.path)
        this.#diff = new DiffRenderable(this.ctx, {
          id: "approval-diff",
          width: "100%",
          height: 8,
          diff: diff.unifiedDiff,
          ...(filetype === undefined ? {} : { filetype }),
          ...(this.#treeSitterClient === undefined
            ? {}
            : { treeSitterClient: this.#treeSitterClient }),
          syntaxStyle: this.#syntaxStyle,
          view: "unified",
          wrapMode: "none",
          showLineNumbers: true,
          addedBg: this.#theme.added,
          removedBg: this.#theme.removed,
          contextBg: this.#theme.panel,
        })
        this.insertBefore(this.#diff, this.select)
      } else {
        this.#diff.diff = diff.unifiedDiff
        this.#diff.filetype = extension(diff.path)
      }
    } else {
      this.#removeDiff()
    }
    this.select.focus()
  }

  #showQuestion(question: QuestionProjection): void {
    this.#activeTool = null
    this.#activeQuestion = question
    this.#activePlan = null
    this.#removeDiff()
    this.visible = true
    this.title = " Rottweiler asks "
    const first = question.questions[0]
    this.prompt.content = first?.prompt ?? "Choose an answer"
    this.select.options = questionOptions(first)
    this.select.setSelectedIndex(0)
    this.select.focus()
  }

  #showPlan(plan: PlanArtifact): void {
    this.#activeTool = null
    this.#activeQuestion = null
    this.#activePlan = plan
    this.#removeDiff()
    this.visible = true
    this.title = " Plan approval required "
    this.prompt.content = `${plan.title}\n${plan.summary_md}\n${plan.steps.length} step${plan.steps.length === 1 ? "" : "s"}`
    this.select.options = [
      { name: "Approve plan", description: "Pin this artifact and enter Execute", value: "approve" },
      { name: "Reject plan", description: "Stay in Plan mode", value: "reject" },
    ]
    this.select.setSelectedIndex(0)
    this.select.focus()
  }

  #selected(index: number): void {
    if (this.#activePlan !== null) {
      const decision: PlanDecision = this.select.options[index]?.value === "approve" ? "approve" : "reject"
      this.#callbacks.onPlanReview(decision)
      return
    }
    if (this.#activeTool !== null) {
      const selected = this.select.options[index]?.value
      const requested: ApprovalDecision =
        selected === "allow_once" || selected === "allow_session" || selected === "allow_project" ? selected : "deny"
      const decision: ApprovalDecision =
        this.#activeTool.diff?.truncated === true ? "deny" : requested
      this.#callbacks.onApproval(this.#activeTool, decision)
      return
    }
    if (this.#activeQuestion !== null) {
      const option = this.select.options[index]
      const value = typeof option?.value === "string" ? option.value : option?.name ?? ""
      this.#callbacks.onAnswer(this.#activeQuestion, [value])
    }
  }

  #removeDiff(): void {
    if (this.#diff !== null) {
      this.remove(this.#diff)
      this.#diff.destroyRecursively()
      this.#diff = null
    }
  }
}

function bashApproval(tool: ToolProjection): { readonly command: string; readonly unsandboxed: boolean } | null {
  if (tool.name !== "bash" || tool.args === null || typeof tool.args !== "object") {
    return null
  }
  const args = tool.args as Record<string, unknown>
  if (typeof args.command !== "string") {
    return null
  }
  return { command: args.command, unsandboxed: args.sandbox === "unsandboxed" }
}

export interface ContextPanelCallbacks {
  readonly onPin: (itemId: string) => void
  readonly onEvict: (itemId: string) => void
}

export class ContextPanelRenderable extends BoxRenderable {
  readonly meter: TextRenderable
  readonly items: SelectRenderable
  #snapshot: ContextSnapshot | null = null
  #callbacks: ContextPanelCallbacks

  constructor(ctx: RenderContext, theme: RottweilerTheme, callbacks: ContextPanelCallbacks) {
    super(ctx, {
      id: "context-panel",
      width: 32,
      height: "100%",
      flexDirection: "column",
      flexShrink: 0,
      border: true,
      borderStyle: "rounded",
      borderColor: theme.border,
      backgroundColor: theme.panel,
      padding: 1,
      title: " Context ",
      titleColor: theme.info,
    })
    this.#callbacks = callbacks
    this.meter = new TextRenderable(ctx, {
      content: "Waiting for context snapshot",
      fg: theme.muted,
      height: 3,
      wrapMode: "word",
    })
    this.items = new SelectRenderable(ctx, {
      width: "100%",
      flexGrow: 1,
      options: [],
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      selectedBackgroundColor: theme.selection,
      selectedTextColor: theme.accentStrong,
      descriptionColor: theme.muted,
      showScrollIndicator: true,
    })
    this.items.onKeyDown = (key) => {
      const item = this.#snapshot?.items[this.items.getSelectedIndex()]
      if (item === undefined) {
        return
      }
      if (key.name === "p") {
        key.preventDefault()
        this.#callbacks.onPin(item.item_id)
      } else if (key.name === "e" || key.name === "delete") {
        key.preventDefault()
        this.#callbacks.onEvict(item.item_id)
      }
    }
    this.add(this.meter)
    this.add(this.items)
  }

  update(state: RottweilerState): void {
    this.#snapshot = state.context
    if (state.context === null) {
      this.meter.content = "Waiting for context snapshot"
      this.items.options = []
      return
    }
    this.meter.content = `${formatPercent(state.context.used_tokens, state.context.usable_tokens)} context · ${state.context.cache_breakpoints.length} cache breaks\nP pin · E evict`
    this.items.options = state.context.items.map((item) => ({
      name: `${item.state.pinned ? "◆" : item.state.evicted ? "×" : "·"} ${item.label}`,
      description: `${item.kind} · ${item.estimated_tokens} tok`,
      value: item.item_id,
    }))
  }
}

export class StatusLineRenderable extends TextRenderable {
  #branch: string | null = null

  constructor(ctx: RenderContext, theme: RottweilerTheme) {
    super(ctx, {
      id: "status-line",
      width: "100%",
      height: 1,
      content: "",
      fg: theme.muted,
      bg: theme.panel,
      truncate: true,
    })
  }

  setBranch(branch: string | null): void {
    this.#branch = branch
  }

  update(state: RottweilerState): void {
    const context =
      state.context === null
        ? "ctx —"
        : `ctx ${formatPercent(state.context.used_tokens, state.context.usable_tokens)}`
    const cache =
      state.cost === null ? "cache —" : `cache ${(state.cost.cache_hit_basis_points / 100).toFixed(0)}%`
    const pluginStatus = Object.entries(state.pluginStatuses).at(-1)
    this.content = [
      ...(state.replay.active ? ["◉ replay"] : []),
      ...(state.replay.active ? [] : [`◉ ${state.mode ?? "execute"}`]),
      `model ${state.model ?? "fast"}`,
      context,
      formatSessionCost(state.cost),
      cache,
      `git ${this.#branch ?? "—"}`,
      ...(pluginStatus === undefined ? [] : [`${pluginStatus[0]} ${pluginStatus[1]}`]),
    ].join("  │  ")
  }
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
      bg: theme.panelRaised,
      visible: false,
      truncate: true,
    })
    this.#theme = theme
  }

  update(state: RottweilerState): void {
    const latestBudget = state.budgets.at(-1)
    const latestError = state.errors.at(-1)
    const latestPluginNotification = state.pluginNotifications.at(-1)
    if (latestError !== undefined) {
      this.visible = true
      this.fg = this.#theme.danger
      this.content = `Error · ${latestError.message}`
    } else if (latestBudget !== undefined && latestBudget.level === "hard_cap") {
      this.visible = true
      this.fg = this.#theme.danger
      this.content = `Budget hard cap · ${latestBudget.scope} ${latestBudget.current}/${latestBudget.limit}`
    } else if (state.replay.active) {
      this.visible = true
      this.fg = this.#theme.info
      const progress =
        state.replay.completedThrough === null
          ? "loading historical events…"
          : `complete through event ${state.replay.completedThrough}`
      this.content = `Replay · ${state.replay.sessionId ?? "historical session"} · read-only · ${progress}`
    } else if (state.compaction.active) {
      this.visible = true
      this.fg = this.#theme.info
      this.content = `Compacting context · ${state.compaction.reason ?? "manual"} · UI remains responsive`
    } else if (state.connection.phase !== "connected" && state.connection.phase !== "idle") {
      this.visible = true
      this.fg = this.#theme.warning
      this.content =
        state.connection.gap === null
          ? `${state.connection.phase} · attempt ${state.connection.attempt}`
          : `Replaying event gap ${state.connection.gap.expected}…${state.connection.gap.received}`
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

function questionOptions(question: Question | undefined) {
  if (question === undefined || question.response_kind === "text") {
    return [{ name: "Write an answer in the composer", description: "Free text", value: "" }]
  }
  return question.options.map((option) => ({
    name: option.label,
    description: option.description ?? "",
    value: option.value,
  }))
}

function readUnifiedDiff(
  value: unknown,
): { path: string; unifiedDiff: string; truncated: boolean } | null {
  if (typeof value !== "object" || value === null) {
    return null
  }
  const record = value as Record<string, unknown>
  return typeof record.path === "string" &&
    typeof record.unified_diff === "string" &&
    typeof record.truncated === "boolean"
    ? { path: record.path, unifiedDiff: record.unified_diff, truncated: record.truncated }
    : null
}

function extension(path: string): string | undefined {
  const name = path.split("/").at(-1) ?? path
  const dot = name.lastIndexOf(".")
  return dot < 0 ? undefined : name.slice(dot + 1)
}
