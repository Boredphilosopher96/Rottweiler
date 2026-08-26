import {
  BoxRenderable,
  DiffRenderable,
  SelectRenderable,
  SelectRenderableEvents,
  TextRenderable,
  bg,
  bold,
  fg,
  t,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"

import {
  commandPreview,
  diffStats,
  filetypeForPath,
  formatStatusContext,
  formatStatusModel,
  formatStatusSessionCost,
  formatToolArguments,
  presentError,
  presentableUnifiedDiff,
} from "../render"
import type {
  ApprovalDecision,
  PermissionModeDescriptor,
  PlanArtifact,
  PlanDecision,
  Question,
} from "../protocol"
import type { QuestionProjection, RottweilerState, ToolProjection } from "../state"
import type { RottweilerTheme } from "../theme"

export interface InteractionCallbacks {
  readonly onApproval: (tool: ToolProjection, action: InteractionApprovalAction) => void
  readonly onAnswer: (question: QuestionProjection, values: readonly string[]) => void
  readonly onPlanReview: (decision: PlanDecision) => void
}

export type InteractionApprovalAction =
  | ApprovalDecision
  | "allow_tool_session"
  | "auto_safe_mode"

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
  readonly leftPane: BoxRenderable
  readonly rightRail: BoxRenderable
  readonly summary: TextRenderable
  readonly rule: TextRenderable
  readonly files: SelectRenderable
  readonly hint: TextRenderable
  readonly diff: DiffRenderable
  readonly details: TextRenderable
  #review: RottweilerState["review"] = null
  #callbacks: ReviewPanelCallbacks
  #theme: RottweilerTheme
  #pendingPaths = new Set<string>()
  #shellActive = false
  #workspaceDiffMode = false
  #terminalWidth: number
  #primaryHeight: number

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    callbacks: ReviewPanelCallbacks,
    treeSitterClient?: TreeSitterClient,
  ) {
    super(ctx, {
      id: "session-review",
      position: "absolute",
      left: 0,
      top: 0,
      width: ctx.width,
      height: ctx.height,
      flexDirection: "row",
      backgroundColor: theme.background,
      overflow: "hidden",
      visible: false,
      zIndex: 20,
    })
    this.#callbacks = callbacks
    this.#theme = theme
    this.#terminalWidth = ctx.width
    this.#primaryHeight = ctx.height
    this.leftPane = new BoxRenderable(ctx, {
      id: "session-review-left",
      width: "100%",
      height: "100%",
      flexDirection: "column",
      backgroundColor: theme.background,
      paddingX: 1,
      overflow: "hidden",
    })
    this.rightRail = new BoxRenderable(ctx, {
      id: "session-review-right",
      width: 37,
      height: "100%",
      flexShrink: 0,
      flexDirection: "column",
      border: ["left"],
      borderStyle: "single",
      borderColor: theme.borderSubtle,
      backgroundColor: theme.backgroundPanel,
      paddingLeft: 1,
      overflow: "hidden",
    })
    this.summary = new TextRenderable(ctx, {
      id: "session-review-summary",
      content: "",
      fg: theme.text,
      height: 1,
      truncate: true,
    })
    this.rule = new TextRenderable(ctx, {
      id: "session-review-rule",
      content: "",
      fg: theme.borderSubtle,
      height: 1,
      truncate: true,
    })
    this.diff = new DiffRenderable(ctx, {
      id: "session-review-diff",
      width: "100%",
      height: 1,
      diff: "",
      ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
      syntaxStyle,
      view: "unified",
      wrapMode: "none",
      showLineNumbers: true,
      addedBg: theme.diffAddedBg,
      removedBg: theme.diffRemovedBg,
      contextBg: theme.backgroundPanel,
    })
    this.files = new SelectRenderable(ctx, {
      id: "session-review-files",
      width: "100%",
      height: 1,
      options: [],
      backgroundColor: theme.backgroundPanel,
      textColor: theme.text,
      selectedBackgroundColor: theme.backgroundElement,
      selectedTextColor: theme.primary,
      descriptionColor: theme.textMuted,
      showScrollIndicator: true,
    })
    this.hint = new TextRenderable(ctx, {
      id: "session-review-hint",
      content: "A accept · R revert",
      fg: theme.textMuted,
      height: 1,
      truncate: true,
    })
    this.details = new TextRenderable(ctx, {
      id: "session-review-details",
      content: "",
      width: "100%",
      height: "100%",
      fg: theme.text,
      wrapMode: "word",
      selectable: true,
    })
    this.files.on(SelectRenderableEvents.SELECTION_CHANGED, () => this.#showSelected())
    this.files.onKeyDown = (key) => {
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
    this.leftPane.add(this.summary)
    this.leftPane.add(this.rule)
    this.leftPane.add(this.files)
    this.leftPane.add(this.diff)
    this.leftPane.add(this.hint)
    this.rightRail.add(this.details)
    this.add(this.leftPane)
    this.add(this.rightRail)
    this.resizeForTerminal(ctx.width, ctx.height)
  }

  /** Own the primary surface and remove the detail rail before it can compress content. */
  resizeForTerminal(
    terminalWidth: number,
    terminalHeight: number,
    primaryHeight = terminalHeight,
  ): void {
    this.#terminalWidth = Math.max(1, terminalWidth)
    this.#primaryHeight = Math.max(1, Math.min(terminalHeight, primaryHeight))
    this.left = 0
    this.top = 0
    this.width = this.#terminalWidth
    this.height = this.#primaryHeight

    const railVisible = this.#terminalWidth >= 110 && this.#primaryHeight >= 12
    const railWidth = railVisible ? 37 : 0
    const leftWidth = this.#terminalWidth - railWidth
    this.leftPane.width = leftWidth
    this.leftPane.height = this.#primaryHeight
    this.rightRail.width = railWidth
    this.rightRail.height = this.#primaryHeight
    this.rightRail.visible = railVisible

    const summaryRows = this.#primaryHeight >= 1 ? 1 : 0
    const ruleRows = this.#primaryHeight >= 4 ? 1 : 0
    const hintRows = this.#primaryHeight >= 2 ? 1 : 0
    const available = Math.max(0, this.#primaryHeight - summaryRows - ruleRows - hintRows)
    const requestedFileRows = this.#workspaceDiffMode || this.#review === null
      ? 0
      : Math.min(5, this.#review.files.length)
    const fileRows = available <= 1 ? available : Math.min(requestedFileRows, available - 1)
    const diffRows = Math.max(0, available - fileRows)

    this.summary.height = summaryRows
    this.summary.visible = summaryRows > 0
    this.rule.height = ruleRows
    this.rule.visible = ruleRows > 0
    this.rule.content = "─".repeat(Math.max(0, leftWidth - 2))
    this.files.height = fileRows
    this.files.visible = fileRows > 0
    this.diff.height = diffRows
    this.diff.visible = diffRows > 0
    this.hint.height = hintRows
    this.hint.visible = hintRows > 0
  }

  update(state: RottweilerState, open = state.review !== null): void {
    this.#shellActive = state.shell.active
    if (!open || state.replay.active) {
      this.#workspaceDiffMode = false
      this.#review = null
      this.visible = false
      return
    }
    if (this.#workspaceDiffMode) {
      // A current-worktree diff is an independent read-only presentation. It
      // must survive unrelated state projections and must never retain a
      // session-review decision target behind the visible diff.
      this.#review = null
      this.visible = true
      this.files.blur()
      this.resizeForTerminal(this.#terminalWidth, this.ctx.height, this.#primaryHeight)
      return
    }
    const review = state.review
    this.#review = review
    if (review === null) {
      this.visible = true
      this.title = " Diff "
      this.summary.content = t`${bold(fg(this.#theme.secondary)("SESSION REVIEW"))}${fg(this.#theme.textMuted)("   loading changes")}`
      this.diff.diff = ""
      this.files.options = []
      this.details.content = ""
      this.hint.content = reviewHint(this.#theme, "close")
      this.resizeForTerminal(this.#terminalWidth, this.ctx.height, this.#primaryHeight)
      return
    }
    const selectedPath = review.files[this.files.getSelectedIndex()]?.path
    const pending = review.files.filter((file) => file.status === "pending").length
    const accepted = review.files.filter((file) => file.status === "accepted").length
    const reverted = review.files.filter((file) => file.status === "reverted").length
    const totals = review.files.reduce(
      (sum, file) => addReviewLineCounts(sum, reviewLineCounts(file.unifiedDiff)),
      { additions: 0, deletions: 0 },
    )
    this.title = ` Session review · ${review.files.length} files `
    this.summary.content = state.shell.active
      ? t`${bold(fg(this.#theme.secondary)("SESSION REVIEW"))}${fg(this.#theme.warning)("   foreground shell active · decisions disabled")}`
      : t`${bold(fg(this.#theme.secondary)("SESSION REVIEW"))}${fg(this.#theme.textMuted)(`   ${review.files.length} files  `)}${fg(this.#theme.diffAdded)(`+${totals.additions}`)}${fg(this.#theme.textMuted)(" ")}${fg(this.#theme.diffRemoved)(`−${totals.deletions}`)}${fg(this.#theme.textMuted)(`   ${pending} pending`)}`
    this.files.options = review.files.map((file) => ({
      name: reviewFileLabel(file),
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
    this.resizeForTerminal(this.#terminalWidth, this.ctx.height, this.#primaryHeight)
    this.#showSelected()
    this.files.focus()
  }

  /** Switch back to the mutable retained session-review presentation. */
  showSessionReview(): void {
    this.#workspaceDiffMode = false
  }

  /** Leave any open presentation before restoring composer focus. */
  closePresentation(): void {
    this.#workspaceDiffMode = false
    this.#review = null
    this.files.blur()
    this.visible = false
  }

  /** Select and reveal an exact retained review path. */
  selectPath(path: string): boolean {
    const index = this.#review?.files.findIndex((file) => file.path === path) ?? -1
    if (index < 0) return false
    this.files.setSelectedIndex(index)
    this.#showSelected()
    if (this.visible) this.files.focus()
    return true
  }

  showDiffMessage(path: string, message: string): void {
    this.visible = true
    this.title = ` Diff · ${path} `
    this.summary.content = t`${bold(fg(this.#theme.secondary)("DIFF"))}${fg(this.#theme.textMuted)(`   ${path}`)}`
    this.diff.diff = ""
    this.files.options = []
    this.details.content = t`${bold(fg(this.#theme.info)("CURRENT VIEW"))}\n${fg(this.#theme.textMuted)("status   ")}${fg(this.#theme.warning)(message)}`
    this.hint.content = reviewHint(this.#theme, "close")
    this.resizeForTerminal(this.#terminalWidth, this.ctx.height, this.#primaryHeight)
  }

  showWorkspaceDiff(
    path: string,
    unifiedDiff: string,
    binary: boolean,
    truncated: boolean,
  ): void {
    this.#workspaceDiffMode = true
    this.#review = null
    this.files.blur()
    this.visible = true
    this.title = ` Diff · ${path} `
    const counts = reviewLineCounts(unifiedDiff)
    this.summary.content = t`${bold(fg(this.#theme.secondary)("DIFF"))}${fg(this.#theme.textMuted)(`   ${path}  `)}${fg(this.#theme.diffAdded)(`+${counts.additions}`)}${fg(this.#theme.textMuted)(" ")}${fg(this.#theme.diffRemoved)(`−${counts.deletions}`)}`
    this.diff.diff = presentableUnifiedDiff(path, unifiedDiff)
    this.diff.filetype = filetypeForPath(path)
    this.files.options = []
    this.details.content = workspaceDiffDetails(this.#theme, path, binary, truncated, counts)
    this.hint.content = reviewHint(this.#theme, "close")
    this.resizeForTerminal(this.#terminalWidth, this.ctx.height, this.#primaryHeight)
  }

  showWorkspaceDiffMessage(path: string, message: string): void {
    this.#workspaceDiffMode = true
    this.#review = null
    this.files.blur()
    this.showDiffMessage(path, message)
  }

  focusPresentation(): void {
    if (this.#workspaceDiffMode) {
      this.files.blur()
    } else {
      this.files.focus()
    }
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
    this.diff.diff = file === undefined
      ? ""
      : presentableUnifiedDiff(file.path, file.unifiedDiff)
    this.diff.filetype = file === undefined ? undefined : filetypeForPath(file.path)
    const revertUnavailable = file !== undefined && file.unrestorableReason !== null
    this.hint.content =
      file === undefined
        ? t`${fg(this.#theme.textMuted)("No files changed in this session")}`
        : this.#shellActive
          ? t`${fg(this.#theme.warning)("Exit the foreground shell before reviewing files")}`
          : this.#pendingPaths.has(file.path)
            ? t`${fg(this.#theme.warning)("Decision pending…")}`
            : reviewHint(this.#theme, revertUnavailable ? "revert-unavailable" : "decide")
    this.#updateDetails(file)
  }

  #updateDetails(file: NonNullable<RottweilerState["review"]>["files"][number] | undefined): void {
    const review = this.#review
    if (review === null || file === undefined) {
      this.details.content = ""
      return
    }
    const counts = reviewLineCounts(file.unifiedDiff)
    const pending = review.files.filter((candidate) => candidate.status === "pending").length
    const accepted = review.files.filter((candidate) => candidate.status === "accepted").length
    const reverted = review.files.filter((candidate) => candidate.status === "reverted").length
    const statusColor = file.status === "accepted"
      ? this.#theme.success
      : file.status === "pending"
        ? this.#theme.warning
        : this.#theme.textMuted
    const revert = file.unrestorableReason === null ? "available" : "unavailable"
    const safety = file.unrestorableReason === null
      ? "A decision is bound to this exact file fingerprint. External edits reopen it."
      : file.unrestorableReason
    this.details.content = t`${bold(fg(this.#theme.info)("THIS FILE"))}\n${fg(this.#theme.textMuted)("status    ")}${fg(statusColor)(file.status)}\n${fg(this.#theme.textMuted)("lines     ")}${fg(this.#theme.diffAdded)(`+${counts.additions}`)}${fg(this.#theme.textMuted)(" ")}${fg(this.#theme.diffRemoved)(`−${counts.deletions}`)}\n${fg(this.#theme.textMuted)("truncated ")}${fg(this.#theme.text)(file.truncated ? "yes" : "no")}\n${fg(this.#theme.textMuted)("revert    ")}${fg(file.unrestorableReason === null ? this.#theme.success : this.#theme.warning)(revert)}\n\n${bold(fg(this.#theme.info)("DECISIONS"))}\n${fg(this.#theme.success)("✓")}${fg(this.#theme.textMuted)(` ${accepted} accepted`)}\n${fg(this.#theme.warning)("○")}${fg(this.#theme.textMuted)(` ${pending} pending`)}\n${fg(this.#theme.textMuted)(`↶ ${reverted} reverted`)}\n\n${bold(fg(this.#theme.info)("SAFETY"))}\n${fg(this.#theme.textMuted)(safety)}`
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

export interface ReviewLineCounts {
  readonly additions: number
  readonly deletions: number
}

export function reviewLineCounts(unifiedDiff: string): ReviewLineCounts {
  const stats = diffStats(presentableUnifiedDiff("file", unifiedDiff))
  return { additions: stats.added, deletions: stats.removed }
}

function addReviewLineCounts(left: ReviewLineCounts, right: ReviewLineCounts): ReviewLineCounts {
  return {
    additions: left.additions + right.additions,
    deletions: left.deletions + right.deletions,
  }
}

function reviewFileLabel(
  file: NonNullable<RottweilerState["review"]>["files"][number],
): string {
  const counts = reviewLineCounts(file.unifiedDiff)
  return `${reviewGlyph(file.status)} ${file.path}  +${counts.additions} −${counts.deletions}`
}

function reviewHint(
  theme: RottweilerTheme,
  mode: "decide" | "revert-unavailable" | "close",
) {
  const key = (label: string) => bg(theme.backgroundElement)(fg(theme.text)(` ${label} `))
  if (mode === "close") return t`${key("esc")}${fg(theme.textMuted)(" close")}`
  return t`${key("a")}${fg(theme.textMuted)(" accept  ")}${key("r")}${fg(theme.textMuted)(mode === "revert-unavailable" ? " revert unavailable  " : " revert  ")}${key("esc")}${fg(theme.textMuted)(" close")}`
}

function workspaceDiffDetails(
  theme: RottweilerTheme,
  path: string,
  binary: boolean,
  truncated: boolean,
  counts: ReviewLineCounts,
) {
  return t`${bold(fg(theme.info)("WORKTREE DIFF"))}\n${fg(theme.textMuted)("path      ")}${fg(theme.text)(path)}\n${fg(theme.textMuted)("lines     ")}${fg(theme.diffAdded)(`+${counts.additions}`)}${fg(theme.textMuted)(" ")}${fg(theme.diffRemoved)(`−${counts.deletions}`)}\n${fg(theme.textMuted)("binary    ")}${fg(theme.text)(binary ? "yes" : "no")}\n${fg(theme.textMuted)("truncated ")}${fg(theme.text)(truncated ? "yes" : "no")}\n\n${bold(fg(theme.info)("MODE"))}\n${fg(theme.textMuted)("read-only current worktree diff")}`
}

function reviewGlyph(status: "pending" | "accepted" | "reverted"): string {
  switch (status) {
    case "pending":
      return "○"
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
  #terminalHeight: number

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
      height: 0,
      maxHeight: 18,
      flexShrink: 0,
      flexDirection: "column",
      overflow: "hidden",
      border: true,
      borderStyle: "rounded",
      borderColor: theme.warning,
      backgroundColor: theme.backgroundElement,
      paddingX: 1,
      visible: false,
      zIndex: 10,
    })
    this.#theme = theme
    this.#syntaxStyle = syntaxStyle
    this.#callbacks = callbacks
    this.#treeSitterClient = treeSitterClient
    this.#terminalHeight = ctx.height
    this.prompt = new TextRenderable(ctx, {
      content: "",
      fg: theme.text,
      wrapMode: "word",
      minHeight: 1,
      flexShrink: 0,
    })
    this.select = new SelectRenderable(ctx, {
      width: "100%",
      height: 0,
      minHeight: 0,
      flexShrink: 0,
      options: [],
      backgroundColor: theme.backgroundElement,
      textColor: theme.text,
      selectedBackgroundColor: theme.backgroundElement,
      selectedTextColor: theme.primary,
      descriptionColor: theme.textMuted,
      wrapSelection: true,
    })
    this.select.on(SelectRenderableEvents.ITEM_SELECTED, (index: number) =>
      this.#selected(index),
    )
    // OpenTUI's SelectRenderable intentionally owns keyboard selection only.
    // A pointer click otherwise changes focus without committing the row, which
    // made permission choices appear inert. Mirror the picker interaction: the
    // press moves the highlight and the matching release activates exactly once.
    this.select.onMouseDown = (event) => {
      if (event.button !== 0) return
      const index = this.#mouseOptionIndex(event.y)
      if (index === null) return
      this.select.setSelectedIndex(index)
      event.preventDefault()
      event.stopPropagation()
    }
    this.select.onMouseUp = (event) => {
      if (event.button !== 0) return
      const index = this.#mouseOptionIndex(event.y)
      if (index === null) return
      this.select.setSelectedIndex(index)
      this.select.selectCurrent()
      event.preventDefault()
      event.stopPropagation()
    }
    this.add(this.prompt)
    this.add(this.select)
  }

  /** Free-text questions deliberately use the composer as the dock input. */
  get usesComposer(): boolean {
    return this.visible && this.#activeQuestion?.questions[0]?.response_kind === "text"
  }

  /** Selectable approvals, questions, and plans own keyboard focus themselves. */
  get capturesInput(): boolean {
    return this.visible && !this.usesComposer
  }

  /**
   * Keep the dock in normal flow and allocate its finite rows explicitly.
   * `reservedRows` belongs to the composer-backed free-text question case.
   */
  resizeForTerminal(terminalHeight: number, reservedRows = 0): void {
    this.#terminalHeight = terminalHeight
    this.#layout(reservedRows)
  }

  update(state: RottweilerState): void {
    if (state.replay.active) {
      this.#activeTool = null
      this.#activeQuestion = null
      this.#activePlan = null
      this.#removeDiff()
      this.visible = false
      this.height = 0
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
      this.#showTool(tool, permissionRuntimeMode(state.permissions))
      return
    }
    if (question !== undefined) {
      this.#showQuestion(question)
      return
    }
    this.#activeTool = null
    this.#activeQuestion = null
    this.#activePlan = null
    this.borderColor = this.#theme.warning
    this.#removeDiff()
    this.visible = false
    this.height = 0
  }

  #showTool(tool: ToolProjection, permissionMode: PermissionModeDescriptor | null): void {
    this.#activeTool = tool
    this.#activeQuestion = null
    this.#activePlan = null
    this.visible = true
    this.select.visible = true
    const bash = bashApproval(tool)
    this.title = bash?.unsandboxed === true ? " UNSANDBOXED approval required " : " Permission required "
    const diff = readUnifiedDiff(tool.diff)
    const truncated = diff?.truncated === true
    const subject = approvalSubject(tool, bash)
    this.prompt.content = [
      subject.line,
      ...(bash === null ? [] : [approvalCommand(bash.command)]),
      ...(subject.available ? [] : [`arguments · ${formatToolArguments(tool.args)}`]),
      ...(truncated
        ? ["Diff exceeds the review limit. Approval is disabled until the complete change can be reviewed."]
        : tool.rationale === null || tool.rationale.trim() === ""
          ? []
          : [tool.rationale]),
    ].join("\n")
    this.select.options = truncated
      ? [{ name: "Deny", description: "A truncated change cannot be approved", value: "deny" }]
      : [
          { name: "Allow once", description: "Run only this invocation", value: "allow_once" },
          { name: "Allow session", description: "Remember for this session", value: "allow_session" },
          { name: "Allow project", description: "Remember this exact invocation in this project", value: "allow_project" },
          { name: `Always allow ${toolDisplayName(tool.name)}`, description: "This session · any arguments", value: "allow_tool_session" },
          ...(permissionMode === "auto-safe" || permissionMode === "yolo"
            ? []
            : [{ name: "Stop asking for safe actions", description: "Switch this session to auto-safe mode", value: "auto_safe_mode" }]),
          { name: "Deny", description: "Do not run the tool", value: "deny" },
        ]
    this.select.setSelectedIndex(0)
    if (diff !== null) {
      if (this.#diff === null) {
        const filetype = filetypeForPath(diff.path)
        this.#diff = new DiffRenderable(this.ctx, {
          id: "approval-diff",
          width: "100%",
          height: 8,
          diff: presentableUnifiedDiff(diff.path, diff.unifiedDiff),
          ...(filetype === undefined ? {} : { filetype }),
          ...(this.#treeSitterClient === undefined
            ? {}
            : { treeSitterClient: this.#treeSitterClient }),
          syntaxStyle: this.#syntaxStyle,
          view: "unified",
          wrapMode: "none",
          showLineNumbers: true,
          addedBg: this.#theme.diffAddedBg,
          removedBg: this.#theme.diffRemovedBg,
          contextBg: this.#theme.backgroundPanel,
        })
        this.insertBefore(this.#diff, this.select)
      } else {
        this.#diff.diff = presentableUnifiedDiff(diff.path, diff.unifiedDiff)
        this.#diff.filetype = filetypeForPath(diff.path)
      }
    } else {
      this.#removeDiff()
    }
    this.#layout()
    this.select.focus()
  }

  #showQuestion(question: QuestionProjection): void {
    this.#activeTool = null
    this.#activeQuestion = question
    this.#activePlan = null
    this.#removeDiff()
    this.visible = true
    this.borderColor = this.#theme.info
    this.title = " Rottweiler asks "
    const first = question.questions[0]
    const freeText = first?.response_kind === "text"
    this.prompt.content = freeText
      ? `${first?.prompt ?? "Your answer"}\nType your answer below. Enter sends; Shift+Enter adds a line.`
      : first?.prompt ?? "Choose an answer"
    this.select.options = questionOptions(first)
    this.select.visible = !freeText
    this.#layout(freeText ? 4 : 0)
    if (!freeText) {
      this.select.setSelectedIndex(0)
      this.select.focus()
    }
  }

  #showPlan(plan: PlanArtifact): void {
    this.#activeTool = null
    this.#activeQuestion = null
    this.#activePlan = plan
    this.#removeDiff()
    this.visible = true
    this.borderColor = this.#theme.info
    this.select.visible = true
    this.title = " Plan approval required "
    this.prompt.content = `${plan.title}\n${plan.summary_md}\n${plan.steps.length} step${plan.steps.length === 1 ? "" : "s"}`
    this.select.options = [
      { name: "Approve plan", description: "Pin this artifact and enter Execute", value: "approve" },
      { name: "Reject plan", description: "Stay in Plan mode", value: "reject" },
    ]
    this.#layout()
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
      const requested: InteractionApprovalAction =
        selected === "allow_once" ||
        selected === "allow_session" ||
        selected === "allow_project" ||
        selected === "allow_tool_session" ||
        selected === "auto_safe_mode"
          ? selected
          : "deny"
      const action: InteractionApprovalAction =
        this.#activeTool.diff?.truncated === true ? "deny" : requested
      this.#callbacks.onApproval(this.#activeTool, action)
      return
    }
    if (this.#activeQuestion !== null) {
      const option = this.select.options[index]
      const value = typeof option?.value === "string" ? option.value : option?.name ?? ""
      this.#callbacks.onAnswer(this.#activeQuestion, [value])
    }
  }

  #mouseOptionIndex(mouseY: number): number | null {
    const localRow = Math.floor(mouseY - this.select.y)
    if (localRow < 0 || localRow >= this.select.height) return null
    // SelectRenderable uses two rows per option when descriptions are visible.
    const scrollOffset = (this.select as unknown as { scrollOffset: number }).scrollOffset
    const index = scrollOffset + Math.floor(localRow / 2)
    return index >= 0 && index < this.select.options.length ? index : null
  }

  #removeDiff(): void {
    if (this.#diff !== null) {
      this.remove(this.#diff)
      this.#diff.destroyRecursively()
      this.#diff = null
    }
  }

  #layout(reservedRows = this.usesComposer ? 4 : 0): void {
    if (!this.visible) {
      this.height = 0
      return
    }

    const promptDesired = Math.min(6, Math.max(1, this.prompt.plainText.split("\n").length))
    const selectDesired = this.select.visible
      ? Math.min(8, Math.max(1, this.select.options.length * 2))
      : 0
    const diffDesired = this.#diff === null ? 0 : 8
    const desiredHeight = 2 + promptDesired + selectDesired + diffDesired
    // Reserve one transcript row and the one-row status line. On extremely
    // short terminals, collapse decorative interaction content before it can
    // paint over the adjacent composer/status surface.
    const terminalLimit = Math.max(0, this.#terminalHeight - 2 - reservedRows)
    const panelHeight = Math.min(18, desiredHeight, terminalLimit)
    this.height = panelHeight

    const framed = panelHeight >= 3
    this.border = framed
    const contentRows = Math.max(0, panelHeight - (framed ? 2 : 0))
    if (contentRows === 0) {
      this.prompt.height = 0
      this.prompt.visible = false
      if (this.#diff !== null) {
        this.#diff.height = 0
        this.#diff.visible = false
      }
      this.select.height = 0
      return
    }

    this.prompt.visible = true
    const hasSelect = this.select.visible
    const promptBudget = hasSelect || this.#diff !== null
      ? Math.max(1, Math.ceil(contentRows * 0.25))
      : contentRows
    const promptRows = Math.min(promptDesired, promptBudget, contentRows)
    this.prompt.height = promptRows
    let remaining = contentRows - promptRows

    let selectRows = 0
    let diffRows = 0
    if (this.#diff !== null) {
      if (hasSelect && remaining > 0) {
        selectRows = Math.min(selectDesired, Math.max(1, Math.ceil(remaining * 0.4)))
      }
      diffRows = Math.max(0, remaining - selectRows)
    } else if (hasSelect) {
      selectRows = Math.min(selectDesired, remaining)
    }

    if (this.#diff !== null) {
      this.#diff.height = diffRows
      this.#diff.visible = diffRows > 0
    }
    this.select.height = selectRows
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

function approvalSubject(
  tool: ToolProjection,
  bash: ReturnType<typeof bashApproval>,
): { readonly line: string; readonly available: boolean } {
  if (bash !== null) return { line: "Run terminal command", available: true }
  const args =
    tool.args !== null && typeof tool.args === "object" && !Array.isArray(tool.args)
      ? tool.args as Record<string, unknown>
      : null
  const primary = ["path", "file_path", "filePath", "command", "pattern", "query"]
    .map((key) => args?.[key])
    .find((value): value is string => typeof value === "string" && value.trim() !== "")
    ?.trim()
  const known = KNOWN_TOOL_DISPLAY_NAMES[tool.name]
  if (known !== undefined) {
    return { line: `${known}${primary === undefined ? "" : ` ${primary}`}`, available: true }
  }
  if (primary !== undefined) {
    return { line: `${toolDisplayName(tool.name)} ${primary}`, available: true }
  }
  return { line: toolDisplayName(tool.name), available: false }
}

function approvalCommand(command: string): string {
  const visible = commandPreview(command).split("\n")
  return [
    `$ ${visible[0] ?? ""}`,
    ...visible.slice(1),
  ].join("\n")
}

export interface ContextPanelCallbacks {
  readonly onOpenDiff?: (path: string) => void
  readonly onOpenSubagent?: (subagentId: string) => void
}

const MAX_SIDEBAR_CHANGED_FILES = 128

export class ContextPanelRenderable extends BoxRenderable {
  readonly agentsTitle: TextRenderable
  readonly agents: SelectRenderable
  readonly todoTitle: TextRenderable
  readonly todos: SelectRenderable
  readonly mcpTitle: TextRenderable
  readonly mcps: SelectRenderable
  readonly runtimeTitle: TextRenderable
  readonly runtimeServices: SelectRenderable
  readonly changedTitle: TextRenderable
  readonly changedFiles: SelectRenderable
  readonly sessionTitle: TextRenderable
  readonly session: TextRenderable
  #callbacks: ContextPanelCallbacks
  readonly #theme: RottweilerTheme
  #agentIds: readonly string[] = []
  #changedPaths: readonly string[] = []
  #activeAgentCount = 0
  #activeMcpCount = 0
  #activeServiceCount = 0
  #showSession = false

  constructor(ctx: RenderContext, theme: RottweilerTheme, callbacks: ContextPanelCallbacks) {
    super(ctx, {
      id: "context-panel",
      width: 36,
      height: "100%",
      flexDirection: "column",
      flexShrink: 0,
      border: ["left"],
      borderStyle: "single",
      borderColor: theme.borderSubtle,
      backgroundColor: theme.background,
      marginRight: 1,
      gap: 0,
    })
    this.#callbacks = callbacks
    this.#theme = theme
    this.agentsTitle = new TextRenderable(ctx, {
      content: "",
      fg: theme.info,
      height: 0,
      flexShrink: 0,
      marginLeft: 1,
      visible: false,
    })
    this.agents = new SelectRenderable(ctx, {
      id: "session-agents",
      width: "100%",
      height: 0,
      flexShrink: 0,
      options: [],
      backgroundColor: theme.background,
      textColor: theme.text,
      selectedBackgroundColor: theme.background,
      selectedTextColor: theme.text,
      descriptionColor: theme.textMuted,
      showScrollIndicator: true,
      showSelectionIndicator: false,
      showDescription: false,
      visible: false,
    })
    this.agents.on(SelectRenderableEvents.ITEM_SELECTED, (index: number) => {
      const subagentId = this.#agentIds[index]
      if (subagentId !== undefined) this.#callbacks.onOpenSubagent?.(subagentId)
    })
    this.todoTitle = new TextRenderable(ctx, {
      content: "TASKS",
      fg: theme.info,
      height: 1,
      flexShrink: 0,
      marginLeft: 1,
    })
    this.todos = new SelectRenderable(ctx, {
      id: "session-todos",
      width: "100%",
      height: "45%",
      options: [],
      backgroundColor: theme.background,
      textColor: theme.text,
      selectedBackgroundColor: theme.background,
      selectedTextColor: theme.text,
      descriptionColor: theme.textMuted,
      showScrollIndicator: true,
      showSelectionIndicator: false,
      showDescription: false,
    })
    this.mcpTitle = new TextRenderable(ctx, {
      content: "MCP",
      fg: theme.info,
      height: 0,
      flexShrink: 0,
      marginLeft: 1,
      visible: false,
    })
    this.mcps = new SelectRenderable(ctx, {
      id: "session-mcp-servers",
      width: "100%",
      height: 0,
      flexShrink: 0,
      options: [],
      backgroundColor: theme.background,
      textColor: theme.text,
      selectedBackgroundColor: theme.background,
      selectedTextColor: theme.text,
      descriptionColor: theme.textMuted,
      showScrollIndicator: true,
      showDescription: false,
      showSelectionIndicator: false,
      visible: false,
    })
    this.runtimeTitle = new TextRenderable(ctx, {
      content: "SERVICES",
      fg: theme.info,
      height: 0,
      flexShrink: 0,
      marginLeft: 1,
      visible: false,
    })
    this.runtimeServices = new SelectRenderable(ctx, {
      id: "session-runtime-services",
      width: "100%",
      height: 0,
      flexShrink: 0,
      options: [],
      backgroundColor: theme.background,
      textColor: theme.text,
      selectedBackgroundColor: theme.background,
      selectedTextColor: theme.text,
      descriptionColor: theme.textMuted,
      showScrollIndicator: true,
      showDescription: false,
      showSelectionIndicator: false,
      visible: false,
    })
    this.changedTitle = new TextRenderable(ctx, {
      content: "CHANGED",
      fg: theme.info,
      height: 1,
      flexShrink: 0,
      marginLeft: 1,
    })
    this.changedFiles = new SelectRenderable(ctx, {
      id: "session-changed-files",
      width: "100%",
      flexGrow: 1,
      options: [],
      backgroundColor: theme.background,
      textColor: theme.text,
      selectedBackgroundColor: theme.background,
      selectedTextColor: theme.text,
      descriptionColor: theme.textMuted,
      showScrollIndicator: true,
      showDescription: false,
      showSelectionIndicator: false,
    })
    this.sessionTitle = new TextRenderable(ctx, {
      content: "",
      fg: theme.info,
      height: 0,
      flexShrink: 0,
      marginLeft: 1,
      visible: false,
    })
    this.session = new TextRenderable(ctx, {
      content: "",
      fg: theme.textMuted,
      height: 0,
      flexShrink: 0,
      marginLeft: 1,
      wrapMode: "none",
      visible: false,
    })
    this.changedFiles.on(SelectRenderableEvents.ITEM_SELECTED, (index: number) => {
      this.#activateChangedFile(index)
    })
    this.changedFiles.onMouseUp = (event) => {
      if (event.button !== 0) return
      const row = Math.floor(event.y - this.changedFiles.y)
      if (row < 0 || row >= this.changedFiles.height) return
      // OpenTUI does not expose row hit-testing for SelectRenderable. Its runtime
      // scroll offset is the only source of truth for mapping a visible mouse row.
      const scrollOffset = (this.changedFiles as unknown as { scrollOffset: number }).scrollOffset
      const index = scrollOffset + row
      this.changedFiles.setSelectedIndex(index)
      this.#activateChangedFile(index)
      event.preventDefault()
      event.stopPropagation()
    }
    this.add(this.agentsTitle)
    this.add(this.agents)
    this.add(this.todoTitle)
    this.add(this.todos)
    this.add(this.changedTitle)
    this.add(this.changedFiles)
    this.add(this.sessionTitle)
    this.add(this.session)
    this.add(this.mcpTitle)
    this.add(this.mcps)
    this.add(this.runtimeTitle)
    this.add(this.runtimeServices)
  }

  update(state: RottweilerState): void {
    const activeAgents = state.subagentOrder
      .map((subagentId) => state.subagents[subagentId])
      .filter((subagent): subagent is NonNullable<typeof subagent> =>
        subagent !== undefined && subagent.status === "running")
    this.#agentIds = activeAgents.map((subagent) => subagent.subagentId)
    this.#activeAgentCount = activeAgents.length
    this.agentsTitle.content = panelHeading(
      this.#theme,
      "AGENTS",
      activeAgents.length === 0 ? "" : `${activeAgents.length} running`,
    )
    this.agents.options = activeAgents.map((subagent) => ({
      name: `${subagentStatusGlyph(subagent.status)} ${subagent.subagentId}  ${subagent.activity ?? subagent.task}`,
      description: "",
      value: subagent.subagentId,
    }))

    const completedTodos = state.todos.filter((todo) => todo.status === "completed").length
    this.todoTitle.content = panelHeading(
      this.#theme,
      "TASKS",
      state.todos.length === 0 ? "" : `${completedTodos}/${state.todos.length}`,
    )
    this.todos.options =
      state.todos.length === 0
        ? [{ name: "○ No tasks", description: "", value: "" }]
        : state.todos.map((todo) => ({
            name: `${todoGlyph(todo.status)} ${todo.content}`,
            description: todo.id,
            value: todo.id,
          }))

    const activeMcps = state.mcpServers.filter((server): server is typeof server & {
      state: { type: "connecting" | "ready" | "stopping" }
    } => server.state.type === "connecting" ||
        server.state.type === "ready" ||
        server.state.type === "stopping")
    this.mcpTitle.visible = activeMcps.length > 0
    this.mcpTitle.height = activeMcps.length > 0 ? 1 : 0
    this.mcps.visible = activeMcps.length > 0
    this.mcps.height = activeMcps.length === 0 ? 0 : Math.min(4, activeMcps.length)
    this.mcps.options = activeMcps.map((server) => ({
      name: `${mcpGlyph(server.state.type)} ${server.name}${server.state.type === "ready" ? ` · ${server.tool_count} tools` : ""}`,
      description: "",
      value: server.name,
    }))

    const activeServices = state.runtimeServices.filter((service) => service.name.length > 0)
    this.runtimeTitle.visible = activeServices.length > 0
    this.runtimeTitle.height = activeServices.length > 0 ? 1 : 0
    this.runtimeServices.visible = activeServices.length > 0
    this.runtimeServices.height = activeServices.length === 0 ? 0 : Math.min(5, activeServices.length)
    this.runtimeServices.options = activeServices.map((service) => ({
      name: `${runtimeServiceLabel(service.kind)} · ${service.name}`,
      description: "",
      value: `${service.kind}:${service.name}`,
    }))
    this.#activeMcpCount = activeMcps.length
    this.#activeServiceCount = activeServices.length

    const reviewPaths = state.review?.files.map((file) => file.path) ?? []
    const statusPaths = state.workspaceStatus?.changedPaths
    const changed = statusPaths === undefined ? null : new Set(statusPaths)
    const candidates =
      statusPaths === undefined
        ? reviewPaths
        : [...reviewPaths.filter((path) => changed?.has(path) === true), ...statusPaths]
    const seen = new Set<string>()
    this.#changedPaths = candidates
      .filter((path) => {
        if (path.length === 0 || seen.has(path)) return false
        seen.add(path)
        return true
      })
      .slice(0, MAX_SIDEBAR_CHANGED_FILES)
    this.changedFiles.options =
      this.#changedPaths.length === 0
        ? [{ name: "○ No changed files", description: "", value: "" }]
        : this.#changedPaths.map((path) => ({ name: `M ${path}`, description: "", value: path }))
    this.changedTitle.content = panelHeading(
      this.#theme,
      "CHANGED",
      this.#changedPaths.length === 0 ? "" : String(this.#changedPaths.length),
    )

    this.#showSession = state.context !== null || state.cost !== null
    this.sessionTitle.content = panelHeading(this.#theme, "SESSION", "")
    const context = state.context === null ? "ctx    —" : formatStatusContext(state.context)
    const cache = state.cost === null
      ? "—"
      : `${(state.cost.cache_hit_basis_points / 100).toFixed(0)}%`
    const cost = state.cost === null
      ? "—"
      : formatStatusSessionCost(state.cost, state.provider, state.context?.used_tokens ?? null)
    this.session.content = t`${fg(this.#theme.textMuted)("ctx    ")}${fg(this.#theme.text)(context.replace(/^ctx\s*/i, ""))}\n${fg(this.#theme.textMuted)("cache  ")}${fg(this.#theme.success)(cache)}\n${fg(this.#theme.textMuted)("spend  ")}${fg(this.#theme.text)(cost)}`
    this.#layoutSectionHeights()
  }

  protected override onResize(_width: number, _height: number): void {
    this.#layoutSectionHeights()
  }

  #layoutSectionHeights(): void {
    const rows = Math.max(1, this.height || this.ctx.height)
    this.gap = 0
    let showAgents = this.#activeAgentCount > 0
    let showSession = this.#showSession
    let showMcp = this.#activeMcpCount > 0
    let showServices = this.#activeServiceCount > 0
    let agentRows = showAgents ? Math.min(3, this.#activeAgentCount) : 0
    let todoRows = Math.max(1, Math.min(4, this.todos.options.length))
    let changedRows = Math.max(1, Math.min(4, this.changedFiles.options.length))
    let sessionRows = showSession ? 3 : 0
    let mcpRows = showMcp ? Math.min(3, this.#activeMcpCount) : 0
    let serviceRows = showServices ? Math.min(3, this.#activeServiceCount) : 0
    const totalRows = () => {
      const sections = 2 + Number(showAgents) + Number(showSession) + Number(showMcp) + Number(showServices)
      return sections + Math.max(0, sections - 1) + agentRows + todoRows + changedRows + sessionRows + mcpRows + serviceRows
    }
    while (totalRows() > rows && changedRows > 1) changedRows -= 1
    while (totalRows() > rows && todoRows > 1) todoRows -= 1
    while (totalRows() > rows && sessionRows > 1) sessionRows -= 1
    while (totalRows() > rows && agentRows > 1) agentRows -= 1
    while (totalRows() > rows && serviceRows > 1) serviceRows -= 1
    while (totalRows() > rows && mcpRows > 1) mcpRows -= 1
    if (totalRows() > rows && showMcp) {
      showMcp = false
      mcpRows = 0
    }
    if (totalRows() > rows && showServices) {
      showServices = false
      serviceRows = 0
    }
    if (totalRows() > rows && showSession) {
      showSession = false
      sessionRows = 0
    }
    if (totalRows() > rows && showAgents) {
      showAgents = false
      agentRows = 0
    }

    this.agentsTitle.visible = showAgents
    this.agentsTitle.height = showAgents ? 1 : 0
    this.agentsTitle.marginTop = 0
    this.agents.visible = showAgents
    this.agents.height = agentRows
    this.todoTitle.marginTop = showAgents ? 1 : 0
    this.todos.height = todoRows
    this.changedTitle.marginTop = 1
    this.changedFiles.flexGrow = 0
    this.changedFiles.height = changedRows
    this.sessionTitle.visible = showSession
    this.sessionTitle.height = showSession ? 1 : 0
    this.sessionTitle.marginTop = showSession ? 1 : 0
    this.session.visible = showSession
    this.session.height = sessionRows
    this.mcpTitle.visible = showMcp
    this.mcpTitle.height = showMcp ? 1 : 0
    this.mcpTitle.marginTop = showMcp ? 1 : 0
    this.mcps.visible = showMcp
    this.mcps.height = mcpRows
    this.runtimeTitle.visible = showServices
    this.runtimeTitle.height = showServices ? 1 : 0
    this.runtimeTitle.marginTop = showServices ? 1 : 0
    this.runtimeServices.visible = showServices
    this.runtimeServices.height = serviceRows
  }

  #activateChangedFile(index: number): void {
    const path = this.#changedPaths[index]
    if (path !== undefined) this.#callbacks.onOpenDiff?.(path)
  }
}

function panelHeading(theme: RottweilerTheme, label: string, meta: string): ReturnType<typeof t> {
  const spacing = " ".repeat(Math.max(1, 34 - label.length - meta.length))
  return t`${bold(fg(theme.info)(label))}${meta === "" ? "" : fg(theme.borderActive)(`${spacing}${meta}`)}`
}

function subagentStatusGlyph(status: RottweilerState["subagents"][string]["status"]): string {
  switch (status) {
    case "running":
      return "◌"
    case "completed":
      return "✓"
    case "failed":
      return "✕"
    case "cancelled":
      return "■"
    case "timed_out":
      return "◷"
    case "max_turns":
      return "◇"
  }
}

function mcpGlyph(state: "connecting" | "ready" | "stopping"): string {
  switch (state) {
    case "connecting":
      return "◌"
    case "ready":
      return "✓"
    case "stopping":
      return "◷"
  }
}

function runtimeServiceLabel(kind: "lsp" | "linter" | "formatter" | "test"): string {
  if (kind === "lsp") return "LSP"
  if (kind === "formatter") return "Format"
  if (kind === "test") return "Test"
  return "Lint"
}

function todoGlyph(status: RottweilerState["todos"][number]["status"]): string {
  switch (status) {
    case "pending":
      return "○"
    case "in_progress":
      return "◌"
    case "completed":
      return "✓"
    case "blocked":
      return "!"
  }
}

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
      state.transcript.length > 0 ||
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
          ? "loading historical events…"
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

const KNOWN_TOOL_DISPLAY_NAMES: Readonly<Record<string, string>> = {
  bash: "Terminal command",
  glob: "Find files",
  grep: "Search files",
  ls: "List files",
  read: "Read file",
  write: "Write file",
  edit: "Edit file",
  multi_edit: "Edit files",
  webfetch: "Open web page",
  websearch: "Search the web",
  ask_user: "Ask a question",
  todo: "Update tasks",
}

function toolDisplayName(name: string): string {
  return KNOWN_TOOL_DISPLAY_NAMES[name] ?? humanLabel(name)
}

function permissionRuntimeMode(
  permissions: RottweilerState["permissions"],
): PermissionModeDescriptor | null {
  return permissions?.runtime_mode ?? null
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

function humanLabel(value: string): string {
  return value.replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase())
}

function userFacingError(category: string, code: string, message: string): string {
  return presentError({ category, code, message }).text
}

function questionOptions(question: Question | undefined) {
  if (question === undefined || question.response_kind === "text") {
    return []
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
