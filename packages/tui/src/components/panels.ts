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

import {
  commandPreview,
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
  Usage,
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
  readonly summary: TextRenderable
  readonly files: SelectRenderable
  readonly hint: TextRenderable
  readonly diff: DiffRenderable
  #review: RottweilerState["review"] = null
  #callbacks: ReviewPanelCallbacks
  #pendingPaths = new Set<string>()
  #shellActive = false
  #workspaceDiffMode = false

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
    this.resizeForTerminal(ctx.height)
  }

  /** Keep the modal and all of its retained children inside the terminal. */
  resizeForTerminal(terminalHeight: number): void {
    const panelHeight = Math.max(4, Math.min(17, terminalHeight - 2))
    this.height = panelHeight
    const contentRows = Math.max(1, panelHeight - 2)
    this.summary.height = 1
    this.summary.visible = true
    this.hint.height = contentRows >= 2 ? 1 : 0
    this.hint.visible = contentRows >= 2
    const remaining = Math.max(0, contentRows - this.summary.height - this.hint.height)
    const diffRows =
      remaining <= 1
        ? remaining
        : Math.max(1, Math.min(remaining - 1, Math.ceil(remaining * 0.6)))
    const fileRows = Math.max(0, remaining - diffRows)
    this.diff.height = diffRows
    this.diff.visible = diffRows > 0
    this.files.height = fileRows
    this.files.visible = fileRows > 0
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
      return
    }
    const review = state.review
    this.#review = review
    if (review === null) {
      this.visible = true
      this.title = " Diff "
      this.summary.content = "Loading changed-file diff…"
      this.diff.diff = ""
      this.files.options = []
      this.hint.content = "Esc close"
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
    this.summary.content = message
    this.diff.diff = ""
    this.files.options = []
    this.hint.content = "Esc close"
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
    this.summary.content = [
      binary ? "Binary file" : "Current worktree diff",
      truncated ? "truncated" : "",
    ].filter(Boolean).join(" · ")
    this.diff.diff = presentableUnifiedDiff(path, unifiedDiff)
    this.diff.filetype = filetypeForPath(path)
    this.files.options = []
    this.hint.content = "Esc close"
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
      backgroundColor: theme.panelRaised,
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
      fg: theme.foreground,
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
          addedBg: this.#theme.added,
          removedBg: this.#theme.removed,
          contextBg: this.#theme.panel,
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
}

const MAX_SIDEBAR_CHANGED_FILES = 128

export class ContextPanelRenderable extends BoxRenderable {
  readonly todoTitle: TextRenderable
  readonly todos: SelectRenderable
  readonly mcpTitle: TextRenderable
  readonly mcps: SelectRenderable
  readonly runtimeTitle: TextRenderable
  readonly runtimeServices: SelectRenderable
  readonly changedTitle: TextRenderable
  readonly changedFiles: SelectRenderable
  #callbacks: ContextPanelCallbacks
  #changedPaths: readonly string[] = []
  #activeMcpCount = 0
  #activeServiceCount = 0

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
      gap: 0,
      title: " Session ",
      titleColor: theme.info,
    })
    this.#callbacks = callbacks
    this.todoTitle = new TextRenderable(ctx, {
      content: "Tasks",
      fg: theme.info,
      height: 1,
      flexShrink: 0,
    })
    this.todos = new SelectRenderable(ctx, {
      id: "session-todos",
      width: "100%",
      height: "45%",
      options: [],
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      selectedBackgroundColor: theme.selection,
      selectedTextColor: theme.accentStrong,
      descriptionColor: theme.muted,
      showScrollIndicator: true,
      showSelectionIndicator: false,
      showDescription: false,
    })
    this.mcpTitle = new TextRenderable(ctx, {
      content: "MCP",
      fg: theme.info,
      height: 0,
      flexShrink: 0,
      visible: false,
    })
    this.mcps = new SelectRenderable(ctx, {
      id: "session-mcp-servers",
      width: "100%",
      height: 0,
      flexShrink: 0,
      options: [],
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      selectedBackgroundColor: theme.selection,
      selectedTextColor: theme.accentStrong,
      descriptionColor: theme.muted,
      showScrollIndicator: true,
      showDescription: false,
      showSelectionIndicator: false,
      visible: false,
    })
    this.runtimeTitle = new TextRenderable(ctx, {
      content: "Services",
      fg: theme.info,
      height: 0,
      flexShrink: 0,
      visible: false,
    })
    this.runtimeServices = new SelectRenderable(ctx, {
      id: "session-runtime-services",
      width: "100%",
      height: 0,
      flexShrink: 0,
      options: [],
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      selectedBackgroundColor: theme.selection,
      selectedTextColor: theme.accentStrong,
      descriptionColor: theme.muted,
      showScrollIndicator: true,
      showDescription: false,
      showSelectionIndicator: false,
      visible: false,
    })
    this.changedTitle = new TextRenderable(ctx, {
      content: "Changed files",
      fg: theme.info,
      height: 1,
      flexShrink: 0,
    })
    this.changedFiles = new SelectRenderable(ctx, {
      id: "session-changed-files",
      width: "100%",
      flexGrow: 1,
      options: [],
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      selectedBackgroundColor: theme.selection,
      selectedTextColor: theme.accentStrong,
      descriptionColor: theme.muted,
      showScrollIndicator: true,
      showDescription: false,
      showSelectionIndicator: false,
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
    this.add(this.todoTitle)
    this.add(this.todos)
    this.add(this.mcpTitle)
    this.add(this.mcps)
    this.add(this.runtimeTitle)
    this.add(this.runtimeServices)
    this.add(this.changedTitle)
    this.add(this.changedFiles)
  }

  update(state: RottweilerState): void {
    this.todos.options =
      state.todos.length === 0
        ? [{ name: "No tasks", description: "", value: "" }]
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
    this.#layoutSectionHeights()

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
        ? [{ name: "No changed files", description: "", value: "" }]
        : this.#changedPaths.map((path) => ({ name: path, description: "", value: path }))
  }

  protected override onResize(_width: number, _height: number): void {
    this.#layoutSectionHeights()
  }

  #layoutSectionHeights(): void {
    const rows = Math.max(1, this.height || this.ctx.height)
    const gap = rows >= 26 ? 1 : 0
    this.gap = gap
    // Rounded border and vertical padding consume four rows. Every visible
    // section reserves one title and at least one data row; service rows then
    // share only the remaining budget so they can never displace changed files.
    const innerRows = Math.max(1, rows - 4)
    let showMcp = this.#activeMcpCount > 0
    let showServices = this.#activeServiceCount > 0
    const minimumRows = () => {
      const sections = 2 + Number(showMcp) + Number(showServices)
      return sections * 2 + gap * Math.max(0, sections * 2 - 1)
    }
    // At unusually short heights, keep the mandatory todo/changed-file sections
    // intact and suppress optional service sections rather than corrupting the
    // border. Prefer the newly requested runtime activity when only one fits.
    if (minimumRows() > innerRows) showMcp = false
    if (minimumRows() > innerRows) showServices = false
    const sectionCount = 2 + Number(showMcp) + Number(showServices)
    const visibleChildren = sectionCount * 2
    this.mcpTitle.visible = showMcp
    this.mcpTitle.height = showMcp ? 1 : 0
    this.mcps.visible = showMcp
    this.runtimeTitle.visible = showServices
    this.runtimeTitle.height = showServices ? 1 : 0
    this.runtimeServices.visible = showServices
    const listBudget = Math.max(
      2,
      innerRows - sectionCount - gap * Math.max(0, visibleChildren - 1),
    )
    const todoRows = Math.max(1, Math.min(4, Math.floor(listBudget / 4)))
    const desiredMcpRows = showMcp ? Math.min(4, this.#activeMcpCount) : 0
    const desiredServiceRows = showServices ? Math.min(5, this.#activeServiceCount) : 0
    const serviceBudget = Math.max(0, listBudget - todoRows - 1)
    let mcpRows = 0
    let serviceRows = 0
    if (showMcp && showServices && serviceBudget >= 2) {
      mcpRows = Math.min(desiredMcpRows, Math.max(1, Math.floor(serviceBudget / 2)))
      serviceRows = Math.min(desiredServiceRows, Math.max(1, serviceBudget - mcpRows))
      const spare = serviceBudget - mcpRows - serviceRows
      if (spare > 0) {
        const addMcp = Math.min(spare, desiredMcpRows - mcpRows)
        mcpRows += addMcp
        serviceRows += Math.min(spare - addMcp, desiredServiceRows - serviceRows)
      }
    } else if (showMcp) {
      mcpRows = Math.min(desiredMcpRows, serviceBudget)
    } else if (showServices) {
      serviceRows = Math.min(desiredServiceRows, serviceBudget)
    }
    const changedRows = Math.max(1, listBudget - todoRows - mcpRows - serviceRows)
    this.todos.height = todoRows
    this.mcps.height = mcpRows
    this.runtimeServices.height = serviceRows
    this.changedFiles.flexGrow = 0
    this.changedFiles.height = changedRows
  }

  #activateChangedFile(index: number): void {
    const path = this.#changedPaths[index]
    if (path !== undefined) this.#callbacks.onOpenDiff?.(path)
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

function runtimeServiceLabel(kind: "lsp" | "linter" | "formatter"): string {
  if (kind === "lsp") return "LSP"
  if (kind === "formatter") return "Format"
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
  #inputMode: "normal" | "insert" | null = null
  #inputTarget: "composer" | "transcript" | "picker" | "interaction" | "review" | null = null
  readonly #modelPickerKeycap: string | null

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    options: { readonly modelPickerKeycap?: string | null } = {},
  ) {
    super(ctx, {
      id: "status-line",
      width: "100%",
      height: 1,
      content: "",
      fg: theme.muted,
      bg: theme.panel,
      truncate: true,
    })
    this.#modelPickerKeycap = options.modelPickerKeycap ?? null
  }

  setBranch(branch: string | null): void {
    this.#branch = branch
  }

  setKeybindingMode(
    mode: "normal" | "insert" | null,
    target: "composer" | "transcript" | "picker" | "interaction" | "review" | null,
  ): void {
    this.#inputMode = mode
    this.#inputTarget = target
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
        : formatStatusContext(state.context)
    const cache =
      state.cost === null
        ? (hasSessionActivity ? "cache —" : null)
        : !hasRecordedUsage(state.cost.session_usage)
        ? "cache —"
        : `cache ${(state.cost.cache_hit_basis_points / 100).toFixed(0)}%`
    const pluginStatus = Object.entries(state.pluginStatuses).at(-1)
    const statusModel = state.model === null
      ? null
      : formatStatusModel(state.model, state.provider, state.models)
    const statusProvider = statusModel?.includes("/") === true
      ? statusModel.slice(0, statusModel.indexOf("/"))
      : state.provider
    this.content = [
      ...(this.#inputMode === null
        ? []
        : [
            `${this.#inputMode === "normal" ? "NORMAL" : "INSERT"}${
              this.#inputTarget === null ? "" : ` · ${this.#inputTarget}`
            }`,
          ]),
      ...(state.replay.active ? ["◉ replay"] : []),
      ...(state.replay.active
        ? []
        : [`◉ ${state.mode ?? "—"}${permissionMode === null ? "" : ` · ${permissionMode}`}`]),
      ...(waitingApproval === undefined ? [] : [`approval · ${toolDisplayName(waitingApproval.name)}`]),
      statusModel === null
        ? `model not selected${
            this.#modelPickerKeycap === null ? "" : ` · ${this.#modelPickerKeycap}`
          }`
        : `model ${statusModel}`,
      ...(context === null ? [] : [context]),
      ...(state.cost === null && !hasSessionActivity
        ? []
        : [formatStatusSessionCost(state.cost, statusProvider, state.context?.used_tokens ?? null)]),
      ...(cache === null ? [] : [cache]),
      ...(this.#branch === null && !hasSessionActivity
        ? []
        : [`git ${this.#branch ?? "—"}`]),
      ...(pluginStatus === undefined ? [] : [`Extension · ${humanLabel(pluginStatus[1])}`]),
    ].join("  │  ")
  }
}

function hasRecordedUsage(usage: Usage): boolean {
  return [
    usage.input_tokens,
    usage.output_tokens,
    usage.cache_read_tokens,
    usage.cache_write_tokens,
    usage.reasoning_tokens,
  ].some((value) => /^(0|[1-9][0-9]*)$/.test(value) && BigInt(value) > 0n)
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
      this.fg = this.#theme.danger
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
