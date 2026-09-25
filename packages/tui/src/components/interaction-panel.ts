import { TextRenderable } from "./text"
import {
  BoxRenderable,
  DiffRenderable,
  ScrollBoxRenderable,
  SelectRenderable,
  SelectRenderableEvents,
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
  readonly onAnswer: (question: QuestionProjection, value: string) => void
  readonly onPlanReview: (decision: PlanDecision) => void
}

/**
 * `always_allow` opens the reviewed project/pattern scope screen;
 * `auto_safe_mode` switches this session to Auto and allows the invocation.
 */
export type InteractionApprovalAction =
  | ApprovalDecision
  | "always_allow"
  | "auto_safe_mode"

const APPROVAL_KEYS: Readonly<Record<string, InteractionApprovalAction>> = {
  y: "allow_once", a: "allow_session", p: "always_allow", n: "deny",
}

export class InteractionPanelRenderable extends BoxRenderable {
  readonly prompt: TextRenderable
  readonly planScroller: ScrollBoxRenderable
  readonly planDetails: TextRenderable
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
    this.planScroller = new ScrollBoxRenderable(ctx, {
      id: "plan-details-scroll", width: "100%", height: 0, visible: false,
      scrollY: true, scrollX: false, flexShrink: 0,
      contentOptions: { flexDirection: "column", width: "100%" },
    })
    this.planDetails = new TextRenderable(ctx, {
      content: "", width: "100%", fg: theme.text, wrapMode: "word", flexShrink: 0,
    })
    this.planScroller.add(this.planDetails)
    this.select.onKeyDown = (key) => {
      if (this.select.focused && this.#activeTool !== null && !key.ctrl && !key.meta && !key.shift
        && !key.super && !key.option && !key.hyper) {
        const index = key.name in APPROVAL_KEYS
          ? this.select.options.findIndex(option => option.value === APPROVAL_KEYS[key.name]
            || (key.name === "a" && option.value === "auto_safe_mode")
            || (key.name === "p" && option.value === "allow_project"))
          : -1
        const action = index >= 0 ? this.select.options[index]?.value : undefined
        if (action !== undefined && index >= 0) {
          this.select.setSelectedIndex(index)
          this.select.selectCurrent()
          key.preventDefault(); key.stopPropagation(); return
        }
      }
      if (this.#activePlan === null || (key.name !== "pageup" && key.name !== "pagedown")) return
      this.planScroller.scrollBy((key.name === "pageup" ? -1 : 1) * Math.max(1, this.planScroller.viewport.height - 1))
      key.preventDefault()
      key.stopPropagation()
    }
    this.add(this.prompt)
    this.add(this.planScroller)
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
    return this.visible && this.#activeQuestion?.question?.response_kind === "text"
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
    this.planScroller.visible = false
    this.planScroller.height = 0
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
    const focus = !this.visible || this.#activeTool?.invocationId !== tool.invocationId
    const selected = this.#retainSelection(["approval", tool.invocationId, tool.turnId, tool.name, tool.args, tool.capabilities, tool.rationale, tool.diff, permissionMode, allowPermissionChanges])
    this.#activeTool = tool
    this.#activeQuestion = null
    this.#activePlan = null
    this.visible = true
    this.select.visible = true
    const bash = bashApproval(tool)
    const diff = readUnifiedDiff(tool.diff)
    const truncated = diff?.truncated === true
    const request = approvalRequest(tool, bash)
    this.title = ` ${request.question} `
    this.borderColor = bash?.unsandboxed === true ? this.#theme.error : this.#theme.warning
    const reason = tool.rationale?.trim() ?? ""
    this.prompt.content = [
      ...request.details,
      ...(truncated
        ? ["The change is too large to review here, so it cannot be approved."]
        : reason === "" ? [] : [reason]),
    ].join("\n")
    this.select.showDescription = false
    this.select.options = truncated
      ? [{ name: "n  No", description: "", value: "deny" }]
      : approvalOptions(tool, request, permissionMode, allowPermissionChanges)
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
    if (focus) this.select.focus()
  }

  #showQuestion(question: QuestionProjection): void {
    this.#activeTool = null
    const focus = !this.visible || this.#activeQuestion?.questionId !== question.questionId
    const selected = this.#retainSelection(["question", question.questionId, question.turnId, question.question])
    this.#activeQuestion = question
    this.#activePlan = null
    this.#removeDiff()
    this.visible = true
    this.borderColor = this.#theme.info
    this.title = " Rottweiler asks "
    const prompt = question.question
    const freeText = prompt.response_kind === "text"
    this.prompt.content = freeText
      ? `${prompt.prompt}\nType your answer below. Enter sends; Shift+Enter adds a line.`
      : prompt.prompt
    this.select.showDescription = true
    this.select.options = questionOptions(prompt)
    this.select.visible = !freeText
    this.#layout(freeText ? 4 : 0)
    if (!freeText) {
      this.select.setSelectedIndex(Math.min(selected, Math.max(0, this.select.options.length - 1)))
      if (focus) this.select.focus()
    }
  }

  #showPlan(plan: PlanArtifact): void {
    this.#activeTool = null
    this.#activeQuestion = null
    const changed = this.#activePlan !== plan
    const selected = this.#retainSelection(["plan", plan])
    this.#activePlan = plan
    this.#removeDiff()
    this.visible = true
    this.borderColor = this.#theme.info
    this.select.visible = true
    this.title = " Plan approval required "
    this.prompt.content = `${plan.title} · PgUp/PgDn to review`
    this.planDetails.content = [
      plan.summary_md,
      ...plan.steps.flatMap((step, index) => [
        `${index + 1}. ${step.description}`,
        ...(step.files_touched.length === 0 ? [] : [`   Files: ${step.files_touched.join(", ")}`]),
        `   Verify: ${step.verification}`,
      ]),
      ...(plan.open_questions.length === 0 ? [] : ["Open questions", ...plan.open_questions.map(question => `• ${question}`)]),
    ].join("\n")
    this.planScroller.visible = true
    if (changed) this.planScroller.scrollTo(0)
    this.select.showDescription = true
    this.select.options = [
      { name: "Approve plan", description: "Pin this artifact and enter Execute", value: "approve" },
      { name: "Reject plan", description: "Stay in Plan mode", value: "reject" },
    ]
    this.#layout()
    this.select.setSelectedIndex(Math.min(selected, Math.max(0, this.select.options.length - 1)))
    if (changed) this.select.focus()
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
          selected === "always_allow" ||
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
      this.#callbacks.onAnswer(this.#activeQuestion, value)
    }
  }

  #mouseOptionIndex(mouseY: number): number | null {
    const localRow = Math.floor(mouseY - this.select.y)
    if (localRow < 0 || localRow >= this.select.height) return null
    // SelectRenderable uses two rows per option when descriptions are visible.
    const scrollOffset = (this.select as unknown as { scrollOffset: number }).scrollOffset
    const index = scrollOffset + Math.floor(localRow / (this.select.showDescription ? 2 : 1))
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

    if (this.#activePlan !== null) {
      const detailRows = this.planDetails.plainText.split("\n").reduce((rows, line) =>
        rows + Math.max(1, Math.ceil(line.length / Math.max(1, (this.width || this.ctx.width) - 4))), 0)
      const panelHeight = Math.min(18, 7 + detailRows, Math.max(0, this.#terminalHeight - 2 - reservedRows))
      this.height = panelHeight
      this.border = panelHeight >= 3
      const rows = Math.max(0, panelHeight - (this.border ? 2 : 0))
      this.prompt.visible = rows > 0
      this.prompt.height = Math.min(1, rows)
      const selectRows = Math.min(4, Math.max(0, rows - 1))
      this.select.height = selectRows
      this.planScroller.height = Math.max(0, rows - 1 - selectRows)
      return
    }
    const promptDesired = this.prompt.plainText === "" && this.#activeTool !== null
      ? 0
      : Math.min(6, Math.max(1, this.prompt.plainText.split("\n").length))
    const selectDesired = this.select.visible
      ? Math.min(8, Math.max(1, this.select.options.length * (this.select.showDescription ? 2 : 1)))
      : 0
    const diffDesired = this.#diff === null ? 0 : Math.min(8, Math.max(1,
      (this.#activeTool?.diff?.unified_diff ?? "").trimEnd().split("\n").filter(line =>
        !line.startsWith("---") && !line.startsWith("+++") && !line.startsWith("diff --git") && !line.startsWith("index ")).length))
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

    const hasSelect = this.select.visible
    this.prompt.visible = promptDesired > 0
    const promptBudget = promptDesired === 0
      ? 0
      : hasSelect || this.#diff !== null
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
      diffRows = Math.min(diffDesired, Math.max(0, remaining - selectRows))
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

interface ApprovalRequest {
  /** The action as a plain question, e.g. "Run `cargo test`?" or "Edit calc.py?". */
  readonly question: string
  /** Lines shown under the question: a long command, or unrecognized arguments. */
  readonly details: readonly string[]
  /** Short name of what "don't ask again" remembers, or null when nothing useful is. */
  readonly remembered: string | null
  readonly fileMutation: boolean
}

const INLINE_COMMAND_CELLS = 60
const REMEMBERED_COMMAND_CELLS = 32
const FILE_MUTATION_TOOLS = new Set(["write", "edit", "multi_edit"])
const TOOL_QUESTIONS: Readonly<Record<string, string>> = {
  write: "Write", edit: "Edit", multi_edit: "Edit", read: "Read", ls: "List",
  glob: "Find files matching", grep: "Search files for", webfetch: "Open", websearch: "Search the web for",
}

function approvalRequest(tool: ToolProjection, bash: ReturnType<typeof bashApproval>): ApprovalRequest {
  if (bash !== null) {
    const preview = commandPreview(bash.command)
    const inline = !preview.includes("\n") && preview.length <= INLINE_COMMAND_CELLS
    const where = bash.unsandboxed ? " outside the sandbox" : ""
    return {
      question: inline ? `Run \`${preview}\`${where}?` : `Run this command${where}?`,
      details: inline ? [] : approvalCommand(preview),
      remembered: inline && preview.length <= REMEMBERED_COMMAND_CELLS ? `\`${preview}\`` : "this command",
      fileMutation: false,
    }
  }
  const args = tool.args !== null && typeof tool.args === "object" && !Array.isArray(tool.args)
    ? tool.args as Record<string, unknown>
    : null
  const primary = ["path", "file_path", "filePath", "url", "command", "pattern", "query"]
    .map((key) => args?.[key])
    .find((value): value is string => typeof value === "string" && value.trim() !== "")
    ?.trim()
  const verb = TOOL_QUESTIONS[tool.name]
  const fileMutation = FILE_MUTATION_TOOLS.has(tool.name)
  if (primary !== undefined) {
    const subject = verb !== undefined && (tool.name === "grep" || tool.name === "websearch") ? `"${primary}"` : primary
    return {
      question: verb === undefined ? `Use ${toolDisplayName(tool.name)} on ${subject}?` : `${verb} ${subject}?`,
      details: [],
      remembered: fileMutation ? null : `${toolDisplayName(tool.name).toLocaleLowerCase()} on ${subject}`,
      fileMutation,
    }
  }
  const known = KNOWN_TOOL_DISPLAY_NAMES[tool.name] !== undefined
  return {
    question: `Use ${toolDisplayName(tool.name)}?`,
    details: known ? [] : [formatToolArguments(tool.args)],
    remembered: fileMutation ? null : "this exact call",
    fileMutation,
  }
}

/**
 * At most four single-line choices, each with its key. "Always allow" opens a
 * reviewed scope screen when rules may change; otherwise it remembers only
 * this exact invocation for the project.
 */
function approvalOptions(
  tool: ToolProjection,
  request: ApprovalRequest,
  permissionMode: PermissionModeDescriptor | null,
  allowPermissionChanges: boolean,
): { name: string; description: string; value: string }[] {
  const autoCoversEdits = request.fileMutation && allowPermissionChanges
    && permissionMode !== "auto-safe" && permissionMode !== "yolo"
    && (tool.rationale === null || tool.rationale.trim() === "")
  const session = autoCoversEdits
    ? { name: "a  Yes, and allow workspace edits this session (Auto)", description: "", value: "auto_safe_mode" }
    : request.remembered === null
      ? null
      : { name: `a  Yes, and don't ask again for ${request.remembered} this session`, description: "", value: "allow_session" }
  const always = allowPermissionChanges
    ? { name: "p  Always allow in this project…", description: "", value: "always_allow" }
    : request.remembered === null
      ? null
      : { name: `p  Always allow ${request.remembered} in this project`, description: "", value: "allow_project" }
  return [
    { name: "y  Yes", description: "", value: "allow_once" },
    ...(session === null ? [] : [session]),
    ...(always === null ? [] : [always]),
    { name: "n  No, and tell the agent what to do differently", description: "", value: "deny" },
  ]
}

function approvalCommand(preview: string): string[] {
  const visible = preview.split("\n")
  return [`$ ${visible[0] ?? ""}`, ...visible.slice(1)]
}

function questionOptions(question: Question) {
  if (question.response_kind === "text") {
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
