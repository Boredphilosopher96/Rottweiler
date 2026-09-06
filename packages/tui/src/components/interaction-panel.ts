import {
  BoxRenderable,
  DiffRenderable,
  SelectRenderable,
  SelectRenderableEvents,
  TextRenderable,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient
} from "@opentui/core"
import type {
  ApprovalDecision,
  PermissionModeDescriptor,
  PlanArtifact,
  PlanDecision,
  Question,
} from "../protocol"
import {
  commandPreview,
  filetypeForPath,
  formatToolArguments,
  presentableUnifiedDiff
} from "../render"
import { interactionFingerprint, type InteractionSelection } from "../interaction-selection"
import type { QuestionProjection, RottweilerState, ToolProjection } from "../state"
import type { RottweilerTheme } from "../theme"
import { KNOWN_TOOL_DISPLAY_NAMES, permissionRuntimeMode, toolDisplayName } from "./panel-labels"

export interface InteractionCallbacks {
  readonly onApproval: (tool: ToolProjection, action: InteractionApprovalAction) => void
  readonly onAnswer: (question: QuestionProjection, values: readonly string[]) => void
  readonly onPlanReview: (decision: PlanDecision) => void
}

export type InteractionApprovalAction =
  | ApprovalDecision
  | "allow_tool_session"
  | "auto_safe_mode"

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
  #selectionSource: readonly unknown[] | null = null
  #selectionFingerprint: string | null = null

  override destroy(): void {
    this.#activeTool = null; this.#activeQuestion = null; this.#activePlan = null; this.#diff = null
    this.#selectionSource = null; this.#selectionFingerprint = null
    super.destroy()
  }

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

  captureSelection(): InteractionSelection | null {
    if (!this.visible || this.#selectionFingerprint === null) return null
    const selected = this.select.getSelectedOption()
    return { composer: this.usesComposer, fingerprint: this.#selectionFingerprint, index: Math.max(0, this.select.options.indexOf(selected!)) }
  }

  restoreSelection(selection: InteractionSelection): boolean {
    if (!this.visible || this.#selectionFingerprint !== selection.fingerprint) return false
    if (!this.usesComposer && selection.index >= this.select.options.length) return false
    if (!this.usesComposer) this.select.setSelectedIndex(selection.index)
    return true
  }

  #retainSelection(source: readonly unknown[]): number {
    const previous = this.captureSelection()
    if (this.#selectionSource === null || source.length !== this.#selectionSource.length
      || source.some((value, index) => value !== this.#selectionSource![index])) {
      this.#selectionFingerprint = interactionFingerprint(source)
      this.#selectionSource = source
    }
    return previous?.fingerprint === this.#selectionFingerprint ? previous.index : 0
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

  update(state: RottweilerState, allowPermissionChanges = true): void {
    if (state.replay.active) {
      this.#activeTool = null
      this.#activeQuestion = null
      this.#activePlan = null
      this.#removeDiff()
      this.visible = false
      this.height = 0
      this.#selectionSource = null; this.#selectionFingerprint = null
      return
    }
    const tool = Object.values(state.tools).find((candidate) => candidate.status === "awaiting_approval")
    const question = Object.values(state.questions)[0]
    const turnRunning = Object.values(state.turns).some((turn) => turn.status === "running")
    if (state.pendingPlan !== null && !turnRunning) {
      this.#showPlan(state.pendingPlan)
      return
    }
    if (tool !== undefined) {
      this.#showTool(tool, permissionRuntimeMode(state.permissions), allowPermissionChanges)
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
    this.#selectionSource = null; this.#selectionFingerprint = null
  }

  #showTool(tool: ToolProjection, permissionMode: PermissionModeDescriptor | null, allowPermissionChanges: boolean): void {
    const selected = this.#retainSelection(["approval", tool.invocationId, tool.turnId, tool.name, tool.args, tool.capabilities, tool.rationale, tool.diff, permissionMode, allowPermissionChanges])
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
        ...(allowPermissionChanges ? [{ name: `Always allow ${toolDisplayName(tool.name)}`, description: "This session · any arguments", value: "allow_tool_session" }] : []),
        ...(!allowPermissionChanges || permissionMode === "auto-safe" || permissionMode === "yolo"
          ? []
          : [{ name: "Stop asking for safe actions", description: "Switch this session to auto-safe mode", value: "auto_safe_mode" }]),
        { name: "Deny", description: "Do not run the tool", value: "deny" },
      ]
    this.select.setSelectedIndex(Math.min(selected, Math.max(0, this.select.options.length - 1)))
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
    const selected = this.#retainSelection(["question", question.questionId, question.turnId, question.questions])
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
      this.select.setSelectedIndex(Math.min(selected, Math.max(0, this.select.options.length - 1)))
      this.select.focus()
    }
  }

  #showPlan(plan: PlanArtifact): void {
    this.#activeTool = null
    this.#activeQuestion = null
    const selected = this.#retainSelection(["plan", plan])
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
    this.select.setSelectedIndex(Math.min(selected, Math.max(0, this.select.options.length - 1)))
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
