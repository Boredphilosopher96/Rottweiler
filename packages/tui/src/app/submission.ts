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
import { parseSessionAction } from "../session-commands"
import type { QuestionProjection, ToolProjection } from "../state"
import type { ChildUiController } from "./children"
import type { SessionUiController } from "./sessions"
import type { PickerContentController } from "./picker-content"
import type { TerminalHandoverAdapter } from "./options"
interface SubmissionHost {
  readonly ui: Pick<RottweilerApp,
    | "closePicker"
    | "composer"
    | "openMcpPicker"
    | "openModelPicker"
    | "openPermissionPicker"
    | "openProviderPicker"
    | "openSettingsPicker"
    | "openSubagentPicker"
    | "openThemePicker"
    | "openTimelinePicker"
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
  #postSubmitPicker: "models" | "providers" | "themes" | "settings" | "permissions" | "mcp" | "agents" | null = null
  constructor(readonly host: SubmissionHost) {}
  get notice(): string | null { return this.#composerNotice }
  set notice(value: string | null) { this.#composerNotice = value }
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
    const scope = this.#scope
    this.host.sessions.clearRewind()
    this.clearComposerNotice()
    if (this.host.ui.state.replay.active) {
      return false
    }
    if (content.startsWith("!")) {
      const originatingSubagentId = this.host.children.activeId
      const accepted = await this.startForegroundShell(content, attachments)
      if (!this.#live(scope)) return accepted
      if (accepted && originatingSubagentId !== null && this.host.children.activeId === originatingSubagentId) {
        this.host.children.leaveSubagent()
      }
      return accepted
    }
    if (this.host.children.activeId !== null) {
      const action = attachments.length === 0 ? parseSessionAction(content) : null
      if (action?.type === "exit") {
        this.host.onExit?.()
        return true
      }
      if (action?.type === "agents") {
        this.#postSubmitPicker = "agents"
        this.host.ui.closePicker()
        return true
      }
      if (attachments.length > 0) {
        this.host.projectError(
          "subagent_attachments_unsupported",
          "Child follow-ups are text-only; remove attachments or return to the parent session.",
        )
        return false
      }
      const subagentId = this.host.children.activeId
      if (this.host.children.subagentDescriptor(subagentId)?.activity === "running") {
        this.host.projectError(
          "subagent_still_running",
          "This child is still working. Inspect its progress or interrupt it before sending a follow-up.",
        )
        return false
      }
      let outcome: void | CommandOutcome | null
      try {
        outcome = await this.host.requests.emit({
          type: "continue_subagent",
          meta: this.host.requests.meta(),
          session_id: this.host.sessionId,
          subagent_id: subagentId,
          content,
        })
    if (!this.#live(scope)) return outcome?.type === "accepted"
      } catch (error) {
        if (!this.#live(scope)) return false
        this.host.projectError(
          "subagent_continue_failed",
          presentError({
            category: "protocol",
            code: "subagent_continue_failed",
            message: safeErrorMessage(error),
          }).text,
          true,
        )
        return false
      }
      if (outcome?.type !== "accepted") {
        if (outcome?.type === "rejected") this.host.projectRejection(outcome)
        else {
          const presentation = presentError({
            category: "protocol",
            code: "subagent_continue_unavailable",
            message: "Couldn't continue the child because the engine connection is unavailable.",
          })
          this.host.projectError("subagent_continue_unavailable", presentation.text, true)
        }
        return false
      }
      this.host.children.responseStarted(subagentId)
      this.host.ui.setState(this.host.ui.state)
      return true
    }
    const textQuestion = Object.values(this.host.ui.state.questions).find(
      (question) => !question.answered && question.questions[0]?.response_kind === "text",
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
        answers: [{ question_id: textQuestion.questionId, values: [content] }],
      })
    if (!this.#live(scope)) return outcome?.type === "accepted"
      if (outcome?.type !== "accepted") {
        this.host.projectRejection(outcome)
        return false
      }
      return true
    }
    const sessionAction = attachments.length === 0 ? parseSessionAction(content) : null
    if (sessionAction?.type === "invalid") {
      this.host.invalidSlash(sessionAction.message)
      return false
    }
    if (sessionAction?.type === "exit") {
      this.host.ui.closePicker()
      this.host.onExit?.()
      return true
    }
    if (sessionAction?.type === "new") {
      this.host.ui.closePicker()
      void this.host.sessions.createSession()
      return true
    }
    if (sessionAction?.type === "rewindTimeline") {
      this.host.ui.closePicker()
      this.host.ui.openTimelinePicker()
      return true
    }
    if (sessionAction?.type === "models") {
      this.#postSubmitPicker = "models"
      this.host.ui.closePicker()
      return true
    }
    if (sessionAction?.type === "providers") {
      this.#postSubmitPicker = "providers"
      this.host.ui.closePicker()
      return true
    }
    if (sessionAction?.type === "agents") {
      this.#postSubmitPicker = "agents"
      this.host.ui.closePicker()
      return true
    }
    if (sessionAction?.type === "theme") {
      this.#postSubmitPicker = "themes"
      this.host.ui.closePicker()
      return true
    }
    if (sessionAction?.type === "settings") {
      this.#postSubmitPicker = "settings"
      this.host.ui.closePicker()
      return true
    }
    if (sessionAction?.type === "permissions") {
      this.#postSubmitPicker = "permissions"
      this.host.ui.closePicker()
      return true
    }
    if (sessionAction?.type === "mcp") {
      this.#postSubmitPicker = "mcp"
      this.host.ui.closePicker()
      return true
    }
    if (sessionAction?.type === "review") {
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
      })
    if (!this.#live(scope)) return outcome?.type === "accepted"
      if (outcome?.type !== "accepted") {
        this.host.reviewOpen = false
        this.host.ui.reviewPanel.closePresentation()
        this.host.ui.setState(this.host.ui.state)
        this.host.projectRejection(outcome)
      }
      return outcome?.type === "accepted"
    }
    if (sessionAction?.type === "fork") {
      return await this.requestFork(sessionAction.atTurn)
    }
    const meta = this.host.requests.meta()
    const outcome = await this.host.requests.emit({
      type: "send_message",
      meta,
      session_id: this.host.sessionId,
      content,
      attachments: [...attachments],
    })
    if (!this.#live(scope)) return outcome?.type === "accepted"
    if (outcome?.type !== "accepted") {
      this.host.projectRejection(outcome)
      return false
    }
    return true
  }

  async startForegroundShell(
    content: string,
    attachments: readonly Attachment[],
  ): Promise<boolean> {
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
    })
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
    const scope = this.#scope
    try {
      const outcome = await this.host.requests.emit({
        type: "approve_tool",
        meta: this.host.requests.meta(),
        session_id: this.host.sessionId,
        tool_call_id: tool.toolCallId,
        invocation_id: tool.invocationId,
        decision,
        binding: approvalBinding(tool.diff),
      })
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

  answer(question: QuestionProjection, values: readonly string[]): void {
    this.host.requests.emit({
      type: "answer_question",
      meta: this.host.requests.meta(),
      session_id: this.host.sessionId,
      question_id: question.questionId,
      answers: [{ question_id: question.questionId, values: [...values] }],
    })
  }

  reviewPlan(decision: PlanDecision): void {
    this.host.requests.emit({
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
      })
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
    const scope = this.#scope
    const meta = this.host.requests.meta()
    this.host.requests.trackFork(meta.request_id)
    const outcome = await this.host.requests.emit({
      type: "fork",
      meta,
      session_id: this.host.sessionId,
      at_turn: atTurn,
      operation_id: crypto.randomUUID(),
    })
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
    const picker = this.#postSubmitPicker
    this.#postSubmitPicker = null
    if (picker === "models") this.host.ui.openModelPicker()
    else if (picker === "providers") this.host.ui.openProviderPicker()
    else if (picker === "themes") this.host.ui.openThemePicker()
    else if (picker === "settings") this.host.ui.openSettingsPicker()
    else if (picker === "permissions") this.host.ui.openPermissionPicker()
    else if (picker === "mcp") this.host.ui.openMcpPicker()
    else if (picker === "agents") this.host.ui.openSubagentPicker()
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
