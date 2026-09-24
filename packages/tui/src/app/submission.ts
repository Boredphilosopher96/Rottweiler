import type { RottweilerApp } from "../app"
import type { ProjectionRequestBroker } from "../projection-requests"
import type {
  ApprovalBinding,
  ApprovalDecision,
  Attachment,
  CommandOutcome,
  PlanDecision,
} from "../protocol"
import { presentError } from "../render"
import { resolveSlashInput, type CatalogScreenName } from "../session-commands"
import type { QuestionProjection, ToolProjection } from "../state"
import type { ChildUiController } from "./children"
import type { SessionUiController } from "./sessions"
import type { PickerContentController } from "./picker-content"
import type { TerminalHandoverAdapter } from "./options"
interface SubmissionHost {
  readonly ui: Pick<RottweilerApp,
    | "closePicker"
    | "composer"
    | "reviewPanel"
    | "setState"
    | "state"
    | "transcript"
  >
  readonly children: ChildUiController
  readonly sessions: SessionUiController
  readonly pickerContent: PickerContentController
  readonly requests: ProjectionRequestBroker
  readonly sessionId: string
  readonly destroyed: boolean
  readonly terminalHandover: TerminalHandoverAdapter | undefined
  reviewOpen: boolean
  admitAnswer(content: string): boolean
  onExit(): void
  onComposerInput(value: string): void
  projectError(code: string, message: string, retryable?: boolean): void
  projectRejection(outcome: void | CommandOutcome | null): void
  invalidSlash(message: string): void
}
export class SubmissionController {
  #scope = {}
  #pendingReviewPaths = new Set<string>()
  #composerNotice: string | null = null
  #lastComposerValue = ""
  #terminalSuspended = false
  #pendingShellTimer: ReturnType<typeof setTimeout> | null = null
  #postSubmitPicker: CatalogScreenName | null = null
  constructor(readonly host: SubmissionHost) {}
  get notice(): string | null { return this.#composerNotice }
  set notice(value: string | null) { this.#composerNotice = value }
  get reviewPending(): boolean { return this.#pendingReviewPaths.size > 0 }
  get terminalSuspended(): boolean { return this.#terminalSuspended }
  restoreInput(value: string): void { this.#lastComposerValue = value }
  reset(): void {
    this.#scope = {}
    this.#pendingReviewPaths.clear()
    this.#composerNotice = null
    this.#postSubmitPicker = null
    this.clearPendingShellTimer()
    if (this.#terminalSuspended && !this.host.destroyed) this.host.terminalHandover?.resume()
    this.#terminalSuspended = false
  }
  #live(scope: object): boolean { return scope === this.#scope && !this.host.destroyed }

  async sendMessage(
    content: string,
    attachments: readonly Attachment[],
  ): Promise<boolean> {
    if (!this.host.admitAnswer(content)) return false
    using replyAllocation = this.host.requests.allocate()
    const scope = this.#scope
    this.host.sessions.clearRewind()
    this.clearComposerNotice()
    if (this.host.ui.state.replay.active) {
      return false
    }
    if (this.host.children.selectedFamily) {
      const question = Object.values(this.host.children.presentedState().questions).find(value => value.question.response_kind === "text")
      if (question !== undefined) {
        if (attachments.length > 0) { this.host.projectError("question_attachments_unsupported", "Answer this question with text only; attachments stay in your draft."); return false }
        return await this.host.children.respond({ type: "question", question_id: question.questionId, answer: { question_id: question.questionId, value: content } })
      }
    }
    if (content.startsWith("!")) return await this.startForegroundShell(content, attachments)
    const textQuestion = Object.values(this.host.ui.state.questions).find(
      (question) => question.question.response_kind === "text",
    )
    if (textQuestion !== undefined) {
      if (attachments.length > 0) {
        this.host.projectError(
          "question_attachments_unsupported",
          "Answer this question with text only; attachments stay in your draft.",
        )
        return false
      }
      const outcome = await this.host.requests.emit({
        type: "answer_question",
        meta: this.host.requests.meta(),
        session_id: this.host.sessionId,
        question_id: textQuestion.questionId,
        answer: { question_id: textQuestion.questionId, value: content },
      }, replyAllocation)
    if (!this.#live(scope)) return outcome?.type === "accepted"
      if (outcome?.type !== "accepted") {
        this.host.projectRejection(outcome)
        return false
      }
      return true
    }
    const slash = attachments.length === 0 ? resolveSlashInput(content) : null
    if (attachments.length === 0) this.host.pickerContent.rememberSlash(content)
    if (slash?.type === "invalid") {
      this.host.invalidSlash(slash.message)
      return false
    }
    if (slash?.type === "screen") {
      this.host.ui.closePicker()
      if (slash.name === "exit") {
        this.host.onExit?.()
        return true
      }
      if (slash.name === "new") {
        void this.host.sessions.createSession()
        return true
      }
      if (slash.name === "review") return await this.#openSessionReview(replyAllocation, scope)
      this.#postSubmitPicker = slash.name
      return true
    }
    const meta = this.host.requests.meta()
    const outcome = await this.host.requests.emit({
      type: "send_message",
      meta,
      session_id: this.host.sessionId,
      content: slash?.type === "engine" ? slash.content : content,
      attachments: [...attachments],
    }, replyAllocation)
    if (!this.#live(scope)) return outcome?.type === "accepted"
    if (outcome?.type !== "accepted") {
      this.host.projectRejection(outcome)
      return false
    }
    return true
  }

  async #openSessionReview(replyAllocation: ReturnType<ProjectionRequestBroker["allocate"]>, scope: object): Promise<boolean> {
    if (this.host.ui.state.shell.active) {
      this.host.projectError(
        "review_unavailable_during_shell",
        "exit the foreground shell before opening session review",
      )
      return false
    }
    this.host.ui.reviewPanel.showSessionReview()
    this.host.reviewOpen = true
    this.host.ui.setState(this.host.ui.state)
    const meta = this.host.requests.issue("review")
    const outcome = await this.host.requests.emit({
      type: "get_session_review",
      meta,
      session_id: this.host.sessionId,
    }, replyAllocation)
    if (!this.#live(scope)) return outcome?.type === "accepted"
    if (outcome?.type !== "accepted") {
      this.host.reviewOpen = false
      this.host.ui.reviewPanel.closePresentation()
      this.host.ui.setState(this.host.ui.state)
      this.host.projectRejection(outcome)
    }
    return outcome?.type === "accepted"
  }

  async startForegroundShell(
    content: string,
    attachments: readonly Attachment[],
  ): Promise<boolean> {
    using replyAllocation = this.host.requests.allocate()
    const scope = this.#scope
    const command = content.slice(1).trim()
    if (command.length === 0 || attachments.length > 0) return false
    this.suspendTerminal()
    this.clearPendingShellTimer()
    this.#pendingShellTimer = setTimeout(() => {
      this.#pendingShellTimer = null
      if (!this.host.ui.state.shell.active) this.resumeTerminal()
    }, 5_000)
    const outcome = await this.host.requests.emit({
      type: "user_shell_started",
      meta: this.host.requests.meta(),
      session_id: this.host.sessionId,
      command,
    }, replyAllocation)
    if (!this.#live(scope)) return outcome?.type === "accepted"
    if (outcome?.type !== "accepted") {
      this.clearPendingShellTimer()
      if (!this.host.ui.state.shell.active) this.resumeTerminal()
      this.host.projectRejection(outcome)
      return false
    }
    return true
  }

  approve(tool: ToolProjection, decision: ApprovalDecision): void {
    void this.submitApproval(tool, decision)
  }

  async submitApproval(tool: ToolProjection, decision: ApprovalDecision): Promise<void> {
    using replyAllocation = this.host.requests.allocate()
    const scope = this.#scope
    try {
      if (tool.status !== "awaiting_approval" || tool.toolCallId === null) throw new Error("tool approval requires an authoritative pending approval")
      if (this.host.children.selectedFamily) {
        await this.host.children.respond({ type: "approval", tool_call_id: tool.toolCallId, invocation_id: tool.invocationId, decision, binding: approvalBinding(tool.diff) })
        return
      }
      const outcome = await this.host.requests.emit({
        type: "approve_tool",
        meta: this.host.requests.meta(),
        session_id: this.host.sessionId,
        tool_call_id: tool.toolCallId,
        invocation_id: tool.invocationId,
        decision,
        binding: approvalBinding(tool.diff),
      }, replyAllocation)
      if (!this.#live(scope)) return
      if (outcome?.type === "rejected") {
        this.host.projectRejection(outcome)
      } else if (outcome === null) {
        this.host.projectError(
          "tool_approval_unavailable",
          `the engine did not acknowledge the ${tool.name} approval decision`,
          true,
        )
      }
    } catch (error) {
      if (!this.#live(scope)) return
      this.host.projectError(
        "tool_approval_failed",
        presentError({
          category: "protocol",
          code: "tool_approval_failed",
          message: safeErrorMessage(error),
        }).text,
        true,
      )
    }
  }

  answer(question: QuestionProjection, value: string): void {
    if (this.host.children.selectedFamily) {
      void this.host.children.respond({ type: "question", question_id: question.questionId, answer: { question_id: question.questionId, value } })
      return
    }
    this.host.requests.dispatch({
      type: "answer_question",
      meta: this.host.requests.meta(),
      session_id: this.host.sessionId,
      question_id: question.questionId,
      answer: { question_id: question.questionId, value },
    })
  }

  reviewPlan(decision: PlanDecision): void {
    if (this.host.children.selectedFamily) {
      void this.host.children.respond({ type: "plan", decision, revisions: decision === "reject" ? "Revise the plan using the user's next message as feedback." : null })
      return
    }
    this.host.requests.dispatch({
      type: "approve_plan",
      meta: this.host.requests.meta(),
      session_id: this.host.sessionId,
      decision,
      revisions: decision === "reject" ? "Revise the plan using the user's next message as feedback." : null,
    })
  }

  async reviewFile(
    path: string,
    currentHash: string,
    decision: "accept" | "revert",
  ): Promise<void> {
    using replyAllocation = this.host.requests.allocate()
    const scope = this.#scope
    if (this.host.ui.state.shell.active) {
      this.host.projectError(
        "review_unavailable_during_shell",
        "exit the foreground shell before deciding session review files",
      )
      return
    }
    if (this.#pendingReviewPaths.has(path)) return
    this.#pendingReviewPaths.add(path)
    this.host.ui.reviewPanel.setDecisionPending(path, true)
    try {
      const outcome = await this.host.requests.emit({
        type: "review_file",
        meta: this.host.requests.meta(),
        session_id: this.host.sessionId,
        path,
        decision,
        current_hash: currentHash,
      }, replyAllocation)
      if (!this.#live(scope)) return
      if (outcome?.type === "rejected") {
        this.host.projectRejection(outcome)
      } else if (outcome === null) {
        this.host.projectError(
          "review_command_unavailable",
          "the review decision was not acknowledged by the engine",
          true,
        )
      }
    } catch {
      if (!this.#live(scope)) return
      this.host.projectError(
        "review_command_failed",
        "the review decision could not be delivered to the engine",
        true,
      )
    } finally {
      if (this.#live(scope)) {
        this.#pendingReviewPaths.delete(path)
        this.host.ui.reviewPanel.setDecisionPending(path, false)
      }
    }
  }

  async requestFork(atTurn: string | null): Promise<boolean> {
    using replyAllocation = this.host.requests.allocate()
    const scope = this.#scope
    const meta = this.host.requests.meta()
    this.host.requests.trackFork(meta.request_id)
    const outcome = await this.host.requests.emit({
      type: "fork",
      meta,
      session_id: this.host.sessionId,
      at_turn: atTurn,
      operation_id: crypto.randomUUID(),
    }, replyAllocation)
    if (!this.#live(scope)) return outcome?.type === "accepted"
    if (outcome === null || outcome?.type === "rejected") {
      this.host.requests.discardFork(meta.request_id)
    }
    if (outcome?.type === "rejected") this.host.projectRejection(outcome)
    return outcome?.type === "accepted"
  }

  suspendTerminal(): void {
    if (this.#terminalSuspended) {
      return
    }
    this.host.terminalHandover?.suspend()
    this.#terminalSuspended = true
  }

  resumeTerminal(): void {
    this.clearPendingShellTimer()
    if (!this.#terminalSuspended) {
      return
    }
    this.host.terminalHandover?.resume()
    this.#terminalSuspended = false
    this.host.ui.composer.focus()
  }

  clearPendingShellTimer(): void {
    if (this.#pendingShellTimer !== null) {
      clearTimeout(this.#pendingShellTimer)
      this.#pendingShellTimer = null
    }
  }

  composerInputChanged(value: string): void {
    const changed = value !== this.#lastComposerValue
    this.#lastComposerValue = value
    if (!changed) {
      this.host.pickerContent.updateComposerAutocomplete(value)
      return
    }
    this.host.onComposerInput?.(value)
    this.host.ui.transcript.clearBlockSelection()
    const hadNotice = this.#composerNotice !== null
    this.#composerNotice = null
    if (hadNotice && !this.host.destroyed) this.host.ui.setState(this.host.ui.state)
    this.host.pickerContent.updateComposerAutocomplete(value)
  }

  clearComposerNotice(): void {
    if (this.#composerNotice === null) return
    this.#composerNotice = null
    if (!this.host.destroyed) this.host.ui.setState(this.host.ui.state)
  }

  openPostSubmitPicker(): void {
    const screen = this.#postSubmitPicker
    this.#postSubmitPicker = null
    if (screen !== null) this.host.pickerContent.openScreen(screen)
  }


}
function safeErrorMessage(error: unknown): string {
  return error instanceof Error && error.message.length > 0
    ? error.message
    : "the request could not be delivered to the engine"
}

function approvalBinding(diff: unknown): ApprovalBinding | null {
  if (typeof diff !== "object" || diff === null) {
    return null
  }
  const value = diff as Record<string, unknown>
  if (
    typeof value.proposal_id !== "string" ||
    typeof value.arguments_hash !== "string" ||
    typeof value.base_hash !== "string" ||
    typeof value.diff_hash !== "string"
  ) {
    return null
  }
  return {
    proposal_id: value.proposal_id,
    arguments_hash: value.arguments_hash,
    base_hash: value.base_hash,
    diff_hash: value.diff_hash,
  }
}
