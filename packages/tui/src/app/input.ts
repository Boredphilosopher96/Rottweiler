import type { KeyEvent } from "@opentui/core"
import type {
  ComposerRenderable,
  TranscriptRenderable,
  ToolsWorkspaceRenderable,
  StatusLineRenderable,
  StateBannerRenderable,
  ReviewPanelRenderable,
  OutputViewerRenderable,
  InteractionPanelRenderable,
  FuzzyPickerRenderable,
} from "../components"
import type { DocumentController } from "../history/document"
import {
  keyStrokeFromEvent,
  legacyMacNavigationAction,
  type CompiledKeybindings,
  type InputMode,
  type VimFocus,
  type KeybindingAction,
  type KeybindingContext,
} from "../keybindings"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker } from "../projection-requests"
import type { CommandOutcome, ModeId } from "../protocol"
import type { RottweilerState } from "../state"
import type { RottweilerTheme } from "../theme"
import { nextModeId } from "../ui-presentation"
import type { ChildUiController } from "./children"
import type { SessionUiController } from "./sessions"
interface NavigableBrowser {
  readonly visible: boolean
  readonly input: { focus(): void }
  moveSelection(direction: 1 | -1): void
  moveToBoundary(end: boolean): void
  activateSelected(): void
}
interface InputUiHost {
  readonly state: RottweilerState
  readonly children: ChildUiController
  readonly sessions: SessionUiController
  readonly document: DocumentController | undefined
  readonly reviewOpen: boolean
  readonly primaryView: "conversation" | "tools"
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly sessionId: string
  readonly destroyed: boolean
  readonly theme: RottweilerTheme
  readonly platform: NodeJS.Platform | undefined
  readonly outputViewer: OutputViewerRenderable
  readonly reviewPanel: ReviewPanelRenderable
  readonly interactionPanel: InteractionPanelRenderable
  readonly composer: ComposerRenderable
  readonly transcript: TranscriptRenderable
  readonly toolsWorkspace: ToolsWorkspaceRenderable
  readonly statusLine: StatusLineRenderable
  readonly banner: StateBannerRenderable
  readonly picker: FuzzyPickerRenderable<unknown>
  readonly mcpBrowser: NavigableBrowser
  readonly settingsBrowser: NavigableBrowser
  readonly themeBrowser: NavigableBrowser
  readonly commandPalette: NavigableBrowser
  discardPendingRestore(): void
  projectRejection(outcome: void | CommandOutcome | null): void
  projectError(code: string, message: string, retryable?: boolean): void
  modelSupportsVision(state: RottweilerState): boolean
  closeOutputViewer(): void
  closeReview(): void
  closePicker(): void
  openSessionPicker(): void
  openSubagentPicker(): void
  openReview(): void
  openCommandPicker(): void
  openModelPicker(): void
  openModePicker(): void
}
export class InputUiController {
  readonly #host: InputUiHost
  #keybindings: CompiledKeybindings
  #inputMode: InputMode
  #vimFocus: VimFocus = "composer"
  #vimFocusBeforePicker: Exclude<VimFocus, "picker"> = "composer"
  #interruptSubagentId: string | null = null
  #interruptEscapeTimer: ReturnType<typeof setTimeout> | null = null
  #interruptEscapeArmed = false
  constructor(host: InputUiHost, bindings: CompiledKeybindings) {
    this.#host = host
    this.#keybindings = bindings
    this.#inputMode = bindings.preset === "vim" ? "normal" : "standard"
  }
  get bindings(): CompiledKeybindings { return this.#keybindings }
  get mode(): InputMode { return this.#inputMode }
  get focus(): VimFocus { return this.#vimFocus }
  get beforePicker(): Exclude<VimFocus, "picker"> { return this.#vimFocusBeforePicker }
  get escapeArmed(): boolean { return this.#interruptEscapeArmed }
  get escapeChild(): string | null { return this.#interruptSubagentId }
  restore(mode: InputMode, focus: Exclude<VimFocus, "picker">): void {
    this.#inputMode = mode; this.restoreFocus(focus)
  }
  restoreFocus(focus: Exclude<VimFocus, "picker">): void { this.#vimFocus = focus; this.#vimFocusBeforePicker = focus }
  modalOpened(): void {
    if (this.#keybindings.preset !== "vim" || this.#vimFocus === "picker") return
    this.#vimFocusBeforePicker = this.#vimFocus; this.#vimFocus = "picker"
    this.setInputMode("insert")
  }
  modalClosed(restoring: boolean): void {
    if (this.#keybindings.preset === "vim") this.#vimFocus = restoring ? "picker" : this.#vimFocusBeforePicker
  }
  onGlobalKey = (key: KeyEvent) => {
    if (this.#host.outputViewer.visible && this.#host.document?.snapshot.open
      && !key.ctrl && !key.meta && !key.shift && (key.name === "left" || key.name === "right")) {
      if (key.name === "left") void this.#host.document.previous()
      else void this.#host.document.next()
      key.preventDefault()
      key.stopPropagation()
      return
    }
    this.#host.discardPendingRestore()
    const focusOwner = this.visibleFocusOwner()
    const plainEscape = keyStrokeFromEvent(key) === "escape"
    if (!plainEscape && this.#interruptEscapeArmed) this.clearInterruptEscape()
    if (
      plainEscape &&
      this.#host.children.activeId !== null &&
      !this.pickerVisible() &&
      !this.#host.outputViewer.visible &&
      !this.#host.reviewOpen
    ) {
      if (this.#keybindings.preset === "vim" && this.#inputMode === "insert") {
        this.setInputMode("normal")
        key.preventDefault()
        key.stopPropagation()
        return
      }
      const subagentId = this.#host.children.activeId
      const running = this.#host.children.subagentDescriptor(subagentId)?.activity === "running"
      this.#host.children.leaveSubagent()
      if (running) this.armInterruptEscape(subagentId)
      key.preventDefault()
      key.stopPropagation()
      return
    }
    if (
      plainEscape &&
      !this.pickerVisible() &&
      !this.#host.outputViewer.visible &&
      !this.#host.reviewOpen &&
      this.isInterruptible()
    ) {
      // In Vim mode the first Escape still leaves insert mode, but it also
      // counts as the first half of the universal double-Escape interrupt.
      if (this.#inputMode === "insert") this.setInputMode("normal")
      if (this.#interruptEscapeArmed) {
        const subagentId = this.#interruptSubagentId
        this.clearInterruptEscape()
        void this.interruptActiveResponse(subagentId)
      } else {
        this.armInterruptEscape()
      }
      key.preventDefault()
      key.stopPropagation()
      return
    }
    if (
      focusOwner === "composer" &&
      !this.pickerVisible() &&
      !this.#host.outputViewer.visible &&
      !this.#host.reviewOpen &&
      !key.ctrl &&
      !key.meta &&
      !key.super &&
      !key.option &&
      !key.hyper &&
      !key.shift &&
      (key.name === "up" || key.name === "down") &&
      this.#host.composer.navigateHistory(key.name === "up" ? "previous" : "next")
    ) {
      if (this.#host.pickerController.anchored) this.#host.closePicker()
      key.preventDefault()
      key.stopPropagation()
      return
    }
    if (
      focusOwner === "interaction" &&
      !key.ctrl &&
      !key.meta &&
      !key.super &&
      !key.option &&
      !key.hyper &&
      !key.shift &&
      (key.name === "return" || key.name === "kpenter" || key.name === "linefeed")
    ) {
      // SelectRenderable handles Return internally but does not normalize the
      // keypad Enter or raw line-feed event on every terminal. Own all shapes
      // at the global priority layer so the focused safety choice is committed
      // exactly once.
      this.#host.interactionPanel.select.selectCurrent()
      key.preventDefault()
      key.stopPropagation()
      return
    }
    const legacyMacNavigation = focusOwner === "composer"
      ? legacyMacNavigationAction(key, this.#host.platform ?? process.platform)
      : null
    if (
      legacyMacNavigation !== null &&
      this.handleKeybindingAction(legacyMacNavigation)
    ) {
      key.preventDefault()
      key.stopPropagation()
      return
    }
    if (
      focusOwner === "composer" &&
      !this.pickerVisible() &&
      key.name === "backspace" &&
      !key.ctrl && !key.meta && !key.option &&
      this.#host.composer.value.length === 0 &&
      this.#host.composer.removeLastAttachment()
    ) {
      key.preventDefault()
      key.stopPropagation()
      return
    }
    const safetyPanelFocused =
      focusOwner === "interaction" || focusOwner === "output" || focusOwner === "review"
    const action =
      focusOwner === "output" || focusOwner === "review"
        ? this.#keybindings.resolve("review", key)
        : focusOwner === "interaction"
          ? null
          : ( this.#keybindings.resolve("global", key) ??
            this.#keybindings.resolve(this.keybindingContext(), key))
    if (action !== null && this.handleKeybindingAction(action)) {
      key.preventDefault()
      key.stopPropagation()
    } else if (
      this.#keybindings.preset === "vim" &&
      this.#inputMode === "normal" &&
      !safetyPanelFocused &&
      !key.ctrl &&
      !key.meta &&
      !key.option
    ) {
      // A focused OpenTUI editor still owns the terminal cursor in normal mode.
      // Never let an unmapped printable/navigation key leak through as text.
      key.preventDefault()
      key.stopPropagation()
    }
  }

  keybindingContext(): KeybindingContext {
    if (this.#keybindings.preset === "standard") {
      return this.#host.outputViewer.visible || this.#host.reviewOpen ? "review" : "standard"
    }
    if (this.modalPickerVisible()) {
      return this.#inputMode === "insert" ? "picker_insert" : "picker_normal"
    }
    if (this.#host.outputViewer.visible || this.#host.reviewOpen) return "review"
    return this.#inputMode === "insert" ? "vim_insert" : "vim_normal"
  }

  handleKeybindingAction(action: KeybindingAction): boolean {
    if (action === "close_overlay") {
      if (this.#host.outputViewer.visible) {
        this.#host.closeOutputViewer()
        return true
      }
      if (this.pickerVisible()) {
        this.#host.closePicker()
        return true
      }
      if (this.#host.reviewOpen) {
        this.#host.closeReview()
        return true
      }
      return false
    }
    if (action === "open_session_picker") {
      this.#host.openSessionPicker()
      return true
    }
    if (action === "new_session") {
      void this.#host.sessions.createSession()
      return true
    }
    if (action === "open_subagent_picker") {
      this.#host.openSubagentPicker()
      return true
    }
    if (action === "block_previous" || action === "block_next" || action === "block_toggle") {
      const focusOwner = this.visibleFocusOwner()
      if (
        (this.#keybindings.preset === "vim" && focusOwner !== "transcript") ||
        (this.#keybindings.preset === "standard" && focusOwner !== "composer")
      ) return false
      const blocks = this.#host.primaryView === "tools" ? this.#host.toolsWorkspace : this.#host.transcript
      if (action === "block_previous") blocks.selectPreviousBlock()
      else if (action === "block_next") blocks.selectNextBlock()
      else blocks.toggleSelectedBlock()
      return true
    }
    if (this.#host.state.replay.active) {
      return this.handleReplayNavigation(action)
    }
    switch (action) {
      case "cycle_agent_mode": {
        const mode: ModeId = nextModeId(this.#host.state.mode, this.#host.state.modes)
        this.#host.requests.emit({
          type: "switch_mode",
          meta: this.#host.requests.meta(),
          session_id: this.#host.sessionId,
          mode,
        })
        return true
      }
      case "open_review":
        this.#host.openReview()
        return true
      case "open_command_picker":
        this.#host.openCommandPicker()
        return true
      case "open_model_picker":
        this.#host.openModelPicker()
        return true
      case "open_mode_picker":
        this.#host.openModePicker()
        return true
      case "paste_image":
        if (!this.#host.modelSupportsVision(this.#host.children.presentedState())) return false
        void this.#host.composer.pasteImage()
        // Let the terminal's normal text-paste path continue. When the
        // clipboard contains an image pasteImage attaches it asynchronously;
        // when it does not, Ctrl-V must remain ordinary text paste.
        return false
      case "open_external_editor":
        if (
          this.pickerVisible() ||
          this.#host.outputViewer.visible ||
          this.#host.reviewOpen
        ) return false
        void this.#host.composer.openExternalEditor()
        return true
      case "enter_normal":
        this.setInputMode("normal")
        return true
      case "enter_insert":
        this.#vimFocus = this.pickerVisible() ? "picker" : "composer"
        this.setInputMode("insert")
        return true
      case "append_insert":
        if (!this.pickerVisible() && this.#vimFocus === "composer") {
          this.#host.composer.editor.moveCursorRight()
        }
        this.#vimFocus = this.pickerVisible() ? "picker" : "composer"
        this.setInputMode("insert")
        return true
      case "focus_next":
        this.cycleVimFocus(1)
        return true
      case "focus_previous":
        this.cycleVimFocus(-1)
        return true
      case "move_left":
        if (this.#vimFocus === "composer") this.#host.composer.editor.moveCursorLeft()
        return true
      case "move_right":
        if (this.#vimFocus === "composer") this.#host.composer.editor.moveCursorRight()
        return true
      case "move_up":
        this.moveVertical(-1)
        return true
      case "move_down":
        this.moveVertical(1)
        return true
      case "word_backward":
        if (this.#vimFocus === "composer") this.#host.composer.editor.moveWordBackward()
        return true
      case "word_forward":
        if (this.#vimFocus === "composer") this.#host.composer.editor.moveWordForward()
        return true
      case "line_start":
        if (this.#vimFocus === "composer") this.#host.composer.editor.gotoLineStart()
        return true
      case "line_end":
        if (this.#vimFocus === "composer") this.#host.composer.editor.gotoLineTextEnd()
        return true
      case "delete_character":
        if (this.#vimFocus === "composer") this.#host.composer.editor.deleteChar()
        return true
      case "page_up":
        this.scrollTranscript(-1, "viewport")
        return true
      case "page_down":
        this.scrollTranscript(1, "viewport")
        return true
      case "view_top":
        if (this.#keybindings.preset === "standard") this.scrollPrimaryTo(0)
        else this.moveToBoundary(false)
        return true
      case "view_bottom":
        if (this.#keybindings.preset === "standard") {
          this.scrollPrimaryTo(this.primaryScrollHeight())
        } else {
          this.moveToBoundary(true)
        }
        return true
      case "select_current":
        if (!this.pickerVisible()) return false
        if (this.#host.themeBrowser.visible) this.#host.themeBrowser.activateSelected()
        else if (this.#host.commandPalette.visible) this.#host.commandPalette.activateSelected()
        else this.#host.picker.select.selectCurrent()
        return true
    }
  }

  handleReplayNavigation(action: KeybindingAction): boolean {
    if (this.#keybindings.preset !== "vim") return false
    switch (action) {
      case "move_up":
        this.scrollTranscript(-1, "step")
        return true
      case "move_down":
        this.scrollTranscript(1, "step")
        return true
      case "page_up":
        this.scrollTranscript(-1, "viewport")
        return true
      case "page_down":
        this.scrollTranscript(1, "viewport")
        return true
      case "view_top":
        this.scrollPrimaryTo(0)
        return true
      case "view_bottom":
        this.scrollPrimaryTo(this.primaryScrollHeight())
        return true
      default:
        return false
    }
  }

  setInputMode(mode: Exclude<InputMode, "standard">): void {
    if (this.#keybindings.preset !== "vim") return
    this.#inputMode = mode
    this.focusForInputMode()
    this.#host.statusLine.setKeybindingMode(mode, this.statusFocusOwner())
    this.#host.composer.setKeybindingMode(mode)
    this.#host.statusLine.update(this.#host.children.presentedState())
  }

  focusForInputMode(): void {
    if (this.#host.outputViewer.visible) {
      this.#host.outputViewer.focusPresentation()
      return
    }
    if (this.#host.reviewPanel.visible) {
      this.#host.reviewPanel.focusPresentation()
      return
    }
    if (this.#host.interactionPanel.capturesInput) {
      this.#host.interactionPanel.select.focus()
      return
    }
    if (this.#host.children.isActiveSubagentRunning()) {
      this.#host.composer.editor.showCursor = false
      this.#host.transcript.scroller.focus()
      return
    }
    if (this.#host.mcpBrowser.visible) {
      this.#host.mcpBrowser.input.focus()
      return
    }
    if (this.#host.settingsBrowser.visible) {
      this.#host.settingsBrowser.input.focus()
      return
    }
    if (this.#host.themeBrowser.visible) {
      this.#host.themeBrowser.input.focus()
      return
    }
    if (this.#host.commandPalette.visible) {
      this.#host.commandPalette.input.focus()
      return
    }
    if (this.#host.picker.visible && !this.#host.pickerController.anchored) {
      if (this.#inputMode === "insert") {
        this.#host.picker.input.focus()
      } else {
        this.#host.picker.select.focus()
      }
      return
    }
    if (this.#inputMode === "standard") {
      this.#host.composer.editor.showCursor = true
      this.#host.composer.focus()
      return
    }
    this.#host.composer.editor.showCursor = this.#inputMode === "insert"
    if (this.#vimFocus === "transcript" || this.#host.state.replay.active) {
      if (this.#host.primaryView === "tools") this.#host.toolsWorkspace.activityScroller.focus()
      else this.#host.transcript.scroller.focus()
    } else {
      this.#host.composer.focus()
    }
  }

  cycleVimFocus(direction: 1 | -1): void {
    if (this.#keybindings.preset !== "vim" || this.pickerVisible()) return
    const targets: readonly Exclude<VimFocus, "picker">[] = ["composer", "transcript"]
    const current = Math.max(0, targets.indexOf(this.#vimFocus as Exclude<VimFocus, "picker">))
    this.#vimFocus = targets[(current + direction + targets.length) % targets.length] ?? "composer"
    this.focusForInputMode()
    this.#host.statusLine.setKeybindingMode("normal", this.#vimFocus)
    this.#host.composer.setKeybindingMode("normal")
    this.#host.statusLine.update(this.#host.children.presentedState())
  }

  moveVertical(direction: 1 | -1): void {
    if (this.#host.mcpBrowser.visible) {
      this.#host.mcpBrowser.moveSelection(direction)
    } else if (this.#host.settingsBrowser.visible) {
      this.#host.settingsBrowser.moveSelection(direction)
    } else if (this.#host.themeBrowser.visible) {
      this.#host.themeBrowser.moveSelection(direction)
    } else if (this.#host.commandPalette.visible) {
      this.#host.commandPalette.moveSelection(direction)
    } else if (this.#host.picker.visible) {
      this.#host.picker.moveSelection(direction)
    } else if (this.#vimFocus === "composer") {
      if (direction < 0) this.#host.composer.editor.moveCursorUp()
      else this.#host.composer.editor.moveCursorDown()
    } else {
      this.scrollTranscript(direction, "step")
    }
  }

  scrollTranscript(direction: 1 | -1, unit: "step" | "viewport"): void {
    if (this.#host.primaryView === "tools") {
      this.#host.toolsWorkspace.activityScroller.scrollBy(direction, unit)
    } else {
      this.#host.transcript.scrollBy(direction, unit)
    }
  }

  moveToBoundary(end: boolean): void {
    if (this.#host.mcpBrowser.visible) {
      this.#host.mcpBrowser.moveToBoundary(end)
    } else if (this.#host.themeBrowser.visible) {
      this.#host.themeBrowser.moveToBoundary(end)
    } else if (this.#host.commandPalette.visible) {
      this.#host.commandPalette.moveToBoundary(end)
    } else if (this.#host.picker.visible) {
      this.#host.picker.moveToBoundary(end)
    } else if (this.#vimFocus === "composer") {
      if (end) this.#host.composer.editor.gotoBufferEnd()
      else this.#host.composer.editor.gotoBufferHome()
    } else {
      this.scrollPrimaryTo(end ? this.primaryScrollHeight() : 0)
    }
  }

  scrollPrimaryTo(position: number): void {
    if (this.#host.primaryView === "tools") this.#host.toolsWorkspace.activityScroller.scrollTo(position)
    else this.#host.transcript.scrollTo(position)
  }

  primaryScrollHeight(): number {
    return this.#host.primaryView === "tools"
      ? this.#host.toolsWorkspace.activityScroller.scrollHeight
      : this.#host.transcript.scroller.scrollHeight
  }

  restoreFocusAfterTranscriptInteraction(): void {
    if (this.#host.destroyed || this.#host.state.replay.active) return
    if (this.#inputMode === "standard") {
      this.focusForInputMode()
      return
    }
    this.#vimFocus = "transcript"
    this.#vimFocusBeforePicker = "transcript"
    this.focusForInputMode()
    this.#host.statusLine.setKeybindingMode(this.#inputMode, "transcript")
    this.#host.composer.setKeybindingMode(this.#inputMode)
    this.#host.statusLine.update(this.#host.children.presentedState())
  }

  visibleFocusOwner(): VimFocus | "interaction" | "output" | "review" {
    if (this.modalPickerVisible()) return "picker"
    if (this.#host.outputViewer.visible) return "output"
    if (this.#host.reviewPanel.visible) return "review"
    if (this.#host.interactionPanel.capturesInput) return "interaction"
    if (this.#host.state.replay.active) return "transcript"
    if (this.#host.children.isActiveSubagentRunning()) return "transcript"
    return this.#vimFocus
  }

  pickerVisible(): boolean {
    return this.#host.mcpBrowser.visible || this.#host.settingsBrowser.visible || this.#host.themeBrowser.visible || this.#host.commandPalette.visible || this.#host.picker.visible
  }

  modalPickerVisible(): boolean {
    return this.#host.mcpBrowser.visible || this.#host.settingsBrowser.visible || this.#host.themeBrowser.visible || this.#host.commandPalette.visible || (this.#host.picker.visible && !this.#host.pickerController.anchored)
  }

  statusFocusOwner(): VimFocus | "interaction" | "review" {
    const owner = this.visibleFocusOwner()
    return owner === "output" ? "review" : owner
  }

  isInterruptible(): boolean {
    return this.#interruptSubagentId !== null ||
      this.#host.state.compaction.active ||
      Object.values(this.#host.state.turns).some((turn) => turn.status === "running")
  }

  armInterruptEscape(subagentId: string | null = null): void {
    this.clearInterruptEscape(false)
    this.#interruptEscapeArmed = true
    this.#interruptSubagentId = subagentId
    this.#host.banner.visible = true
    this.#host.banner.fg = this.#host.theme.warning
    this.#host.banner.content = subagentId === null
      ? "Press Esc again to stop the active response"
      : "Back in parent · press Esc again to stop the child agent"
    this.#interruptEscapeTimer = setTimeout(() => this.clearInterruptEscape(), 900)
  }

  clearInterruptEscape(refresh = true): void {
    if (this.#interruptEscapeTimer !== null) {
      clearTimeout(this.#interruptEscapeTimer)
      this.#interruptEscapeTimer = null
    }
    if (!this.#interruptEscapeArmed) return
    this.#interruptEscapeArmed = false
    this.#interruptSubagentId = null
    if (refresh && !this.#host.destroyed) this.#host.banner.update(this.#host.state)
  }

  async interruptActiveResponse(subagentId: string | null = this.#interruptSubagentId): Promise<void> {
    if (subagentId !== null) {
      this.#interruptSubagentId = null
      await this.#host.children.interruptSubagent(subagentId)
      return
    }
    const outcome = await this.#host.requests.emit({
      type: "interrupt",
      meta: this.#host.requests.meta(),
      session_id: this.#host.sessionId,
    })
    if (outcome === null) {
      this.#host.projectError(
        "interrupt_unavailable",
        "Couldn't stop the active response because the engine connection is unavailable.",
        true,
      )
      return
    }
    this.#host.projectRejection(outcome)
  }

}
