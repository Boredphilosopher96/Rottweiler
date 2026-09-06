import { TextRenderable } from "./text"
import {
  BoxRenderable,
  DiffRenderable,
  SelectRenderable,
  SelectRenderableEvents,
  bg,
  bold,
  fg,
  t,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"
import {
  diffStats,
  filetypeForPath,
  presentableUnifiedDiff
} from "../render"
import type { RottweilerState } from "../state"
import type { RottweilerTheme } from "../theme"

export type ReviewFileDecision = "accept" | "revert"

export interface ReviewPanelCallbacks {
  readonly onDecision: (
    file: NonNullable<RottweilerState["review"]>["files"][number],
    decision: ReviewFileDecision,
  ) => void
  readonly onClose: () => void
}

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

  override destroy(): void {
    this.#review = null
    super.destroy()
  }

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

  resetSession(): void {
    this.#pendingPaths.clear()
    this.closePresentation()
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
