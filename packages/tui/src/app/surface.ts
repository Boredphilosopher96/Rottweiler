import { BoxRenderable, type RenderContext, type TreeSitterClient } from "@opentui/core"
import type { RottweilerApp } from "../app"
import {
  ComposerRenderable,
  ContextPanelRenderable,
  PickerScreenRenderable,
  InteractionPanelRenderable,
  ListDetailRenderable,
  OutputViewerRenderable,
  ReviewPanelRenderable,
  StateBannerRenderable,
  StatusLineRenderable,
  AgentsStripRenderable,
  ToolsWorkspaceRenderable,
  TranscriptRenderable,
} from "../components"
import type { DocumentController } from "../history/document"
import type { HistoryPresentation } from "../history/presentation"
import { mcpBrowserRow, type McpBrowserAction } from "../mcp-browser"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker } from "../projection-requests"
import type { Attachment } from "../protocol"
import type { SettingsBrowserAction } from "../settings-browser"
import type { createSyntaxStyle, RottweilerTheme } from "../theme"
import type { ChildUiController } from "./children"
import type { InputUiController } from "./input"
import type { RottweilerAppOptions } from "./options"
import type { PaletteAction, PickerContentController } from "./picker-content"
import type { SessionUiController } from "./sessions"
import type { SubmissionController } from "./submission"
import { themeBrowserDetail, themeBrowserRow } from "./themes"
import { paletteRow } from "../components/slash-popup"
import type { AgentsBrowserAction } from "../agents-browser"
import type { SkillsBrowserAction } from "../skills-browser"
interface SurfaceHost {
  readonly ui: Pick<RottweilerApp,
    | "add"
    | "banner"
    | "commandPalette"
    | "composer"
    | "contextPanel"
    | "interactionPanel"
    | "main"
    | "mcpBrowser"
    | "openAttachmentPicker"
    | "openFilePicker"
    | "openAlwaysAllowReview"
    | "outputViewer"
    | "picker"
    | "primaryView"
    | "reviewPanel"
    | "setState"
    | "settingsBrowser"
    | "state"
    | "statusLine"
    | "agentsStrip"
    | "agentsBrowser"
    | "skillsBrowser"
    | "themeBrowser"
    | "toolsWorkspace"
    | "transcript"
  >
  retryTodos(): void
  readonly context: RenderContext
  readonly width: number
  readonly height: number
  readonly options: RottweilerAppOptions & Required<Pick<RottweilerAppOptions, "editor" | "imagePaste">>
  readonly syntaxStyle: ReturnType<typeof createSyntaxStyle>
  readonly treeSitterClient: TreeSitterClient | undefined
  readonly input: InputUiController
  readonly children: ChildUiController
  readonly history: HistoryPresentation
  readonly document: DocumentController
  readonly requests: ProjectionRequestBroker
  readonly submission: SubmissionController
  readonly pickerController: PickerController
  readonly sessions: SessionUiController
  readonly pickerContent: PickerContentController
  outputViewerInvocationId: string | null
  openToolOutput(id: string): void
  openChangedFileDiff(path: string): void
  closeReview(): void
  resizeNavigation(): void
  resizeReviewPanel(width: number, height: number): void
  projectError(code: string, message: string, retryable?: boolean): void
  onSubmit(content: string, attachments: readonly Attachment[]): Promise<boolean>
  onSubmissionSettled(): void
  onInputSettled(): void
}

/** Construct the retained component tree and bind its interaction ports. */
export function buildSurface(host: SurfaceHost, theme: RottweilerTheme): void {
    host.ui.banner = new StateBannerRenderable(host.context, theme)
    host.ui.main = new BoxRenderable(host.context, {
      id: "main-content",
      width: "100%",
      flexGrow: 1,
      minHeight: 1,
      flexDirection: "row",
      backgroundColor: theme.background,
      gap: 0,
    })
    host.ui.transcript = new TranscriptRenderable(host.context, theme, {
      diagnostics: host.options.diagnostics,
      syntaxStyle: host.syntaxStyle,
      ...(host.treeSitterClient === undefined
        ? {}
        : { treeSitterClient: host.treeSitterClient }),
      onInteraction: () => host.input.restoreFocusAfterTranscriptInteraction(),
      onOpenSubagent: (subagentId) => {
        void host.children.enterSubagent(subagentId)
      },
      childAgentName: id => host.children.subagentDescriptor(id)?.agent || null,
      onOpenChild: child => {
        host.children.openHistorical({ sessionId: child.session_id, subagentId: child.subagent_id, task: child.task.text, sourceSequence: child.task.source.sequence })
      },
      onOpenLiveContent: source => {
        host.outputViewerInvocationId = null
        void host.document.openSource(host.children.readTarget, source)
        host.ui.setState(host.ui.state)
        host.ui.outputViewer.focusPresentation()
      },
      onOpenContent: source => {
        const view = host.history?.controller.snapshot.page?.view
        if (view === undefined || host.document === null) return
        host.outputViewerInvocationId = null
        void host.document.open(host.children.readTarget, view, source)
        host.ui.setState(host.ui.state)
        host.ui.outputViewer.focusPresentation()
      },
      onHistoryAnchor: anchor => host.history.controller.setAnchor(anchor),
      onHistorySeek: ordinal => { void host.history?.controller.seek(ordinal) },
      onHistoryAround: item => host.history.controller.around(item),
      onHistorySearch: source => host.history.controller.load({ type: "search_match", source }),
      onHistoryBoundary: boundary => { void host.history?.controller.load({ type: boundary }) },
      onHistoryFollowing: following => host.history?.controller.setFollowing(following),
      onOpenToolOutput: (invocationId) => host.openToolOutput(invocationId),
    })
    host.ui.toolsWorkspace = new ToolsWorkspaceRenderable(host.context, theme, {
      onOpenToolOutput: (invocationId) => host.openToolOutput(invocationId),
    })
    host.ui.toolsWorkspace.visible = host.ui.primaryView === "tools"
    host.ui.contextPanel = new ContextPanelRenderable(host.context, theme, {
      onRetryTodos: () => host.retryTodos(),
      onOpenDiff: (path) => host.openChangedFileDiff(path),
      onOpenSubagent: (subagentId) => {
        void host.children.enterSubagent(subagentId)
      },
    })
    host.ui.main.add(host.ui.transcript)
    host.ui.main.add(host.ui.toolsWorkspace)
    host.ui.main.add(host.ui.contextPanel)
    host.ui.interactionPanel = new InteractionPanelRenderable(
      host.context,
      theme,
      host.syntaxStyle,
      {
        onApproval: (tool, action) => {
          if (action === "always_allow") {
            host.ui.openAlwaysAllowReview(tool)
          } else if (action === "auto_safe_mode") {
            void host.submission.sendMessage("/permissions mode auto-safe", [])
            host.submission.approve(tool, "allow_once")
          } else {
            host.submission.approve(tool, action)
            // "No, and tell the agent what to do differently": the composer
            // takes the correction as the next message.
            if (action === "deny") host.ui.composer.focus()
          }
        },
        onAnswer: (question, values) => host.submission.answer(question, values),
        onPlanReview: (decision) => host.submission.reviewPlan(decision),
      },
      host.treeSitterClient,
    )
    host.ui.reviewPanel = new ReviewPanelRenderable(
      host.context,
      theme,
      host.syntaxStyle,
      {
        onDecision: (file, decision) =>
          void host.submission.reviewFile(file.path, file.currentHash, decision),
        onClose: () => host.closeReview(),
      },
      host.treeSitterClient,
    )
    host.ui.outputViewer = new OutputViewerRenderable(host.context, theme)
    host.ui.agentsStrip = new AgentsStripRenderable(
      host.context,
      theme,
      (subagentId) => void host.children.enterSubagent(subagentId),
      () => {
        if (host.children.activeId !== null) host.children.updateSubagentBanner(host.children.presentedState())
      },
    )
    const screen = { surfaceLayout: "primary", splitListWidth: 48, splitMinWidth: 96, surfaceBackground: theme.background } as const
    host.ui.agentsBrowser = new ListDetailRenderable<AgentsBrowserAction>(host.context, theme, {
      ...screen, inputPlaceholder: "Filter agents…", emptyCopy: "No child agents yet",
    })
    host.ui.skillsBrowser = new ListDetailRenderable<SkillsBrowserAction>(host.context, theme, {
      ...screen, inputPlaceholder: "Filter skills, commands, and agents…", emptyCopy: "No matching skills",
    })
    const picker = new PickerScreenRenderable<unknown>(host.context, theme, (query) => {
      if (host.ui.picker !== picker) return
      if (host.pickerController.kind === "sessions") host.sessions.scheduleSessionSearch(query)
    })
    host.ui.picker = picker
    host.ui.commandPalette = new ListDetailRenderable<PaletteAction>(host.context, theme, {
      surfaceLayout: "primary", surfaceBackground: theme.background, splitMinWidth: 90,
      inputPlaceholder: "Search commands…",
      renderRow: (row, selected, width) => paletteRow(row.action, row.matchSpans, selected, width, theme),
    })
    host.ui.mcpBrowser = new ListDetailRenderable<McpBrowserAction>(host.context, theme, {
      surfaceLayout: "primary",
      splitListWidth: 72,
      splitMinWidth: 108,
      inputPlaceholder: "Filter MCP connections…",
      emptyCopy: "No MCP servers configured",
      surfaceBackground: theme.background,
      renderRow: (row, selected) => {
        const action = row.action
        const server = action.kind === "manage"
          ? host.ui.state.mcpServers.find((candidate) => candidate.name === action.server)
          : undefined
        return mcpBrowserRow(row, server, selected, theme)
      },
    })
    host.ui.settingsBrowser = new ListDetailRenderable<SettingsBrowserAction>(host.context, theme, {
      surfaceLayout: "primary",
      splitListWidth: 29,
      splitMinWidth: 90,
      inputPlaceholder: "Filter settings…",
      emptyCopy: "No matching settings",
      surfaceBackground: theme.background,
    })
    host.ui.themeBrowser = new ListDetailRenderable<RottweilerTheme>(host.context, theme, {
      surfaceLayout: "primary",
      splitListWidth: 33,
      splitMinWidth: 100,
      compactMinHeight: 8,
      inputPlaceholder: "Filter themes…",
      emptyCopy: "No matching themes",
      surfaceBackground: theme.background,
      renderRow: (row, selected, availableWidth) =>
        themeBrowserRow(row, selected, availableWidth, theme),
      renderDetail: (row) => themeBrowserDetail(row.action),
    })
    const composerContexts = ["global", host.pickerContent.composerKeybindingContext()] as const
    host.ui.composer = new ComposerRenderable(host.context, theme, {
      editor: host.options.editor,
      imagePaste: host.options.imagePaste,
      hintKeys: {
        palette: host.pickerContent.bindingHint("open_command_picker", ["global"]),
        model: host.pickerContent.bindingHint("open_model_picker", ["global"]),
        editor: host.pickerContent.bindingHint("open_external_editor", composerContexts),
        pasteImage: host.pickerContent.bindingHint("paste_image", composerContexts),
        background: host.pickerContent.bindingHint("background_subagent", ["global"]),
      },
      onSubmit: host.onSubmit,
      submissionScope: () => host.children.composerScope(),
      drafts: host.children.draftStore,
      onFileMention: (mention) => host.ui.openFilePicker(mention.query, true),
      onManageAttachments: () => host.ui.openAttachmentPicker(),
      onAttachmentError: (message) =>
        host.projectError("attachment_unavailable", message, true),
      onInput: (value) => host.submission.composerInputChanged(value),
      onSubmitted: () => host.submission.openPostSubmitPicker(),
      onSubmissionSettled: host.onSubmissionSettled,
      onInputSettled: host.onInputSettled,
      onHeightChange: (height) => {
        if (host.ui.composer !== undefined && host.ui.statusLine !== undefined) host.resizeNavigation()
        host.ui.interactionPanel.resizeForTerminal(
          host.height,
          host.ui.interactionPanel.usesComposer ? height : 0,
        )
        if (host.ui.reviewPanel.visible && host.ui.statusLine !== undefined) {
          host.resizeReviewPanel(
            host.width,
            host.height,
          )
        }
      },
    })
    host.ui.statusLine = new StatusLineRenderable(host.context, theme, {
      modelPickerKeycap: host.pickerContent.bindingHint("open_model_picker", ["global"]),
    })
    host.ui.add(host.ui.banner)
    host.ui.add(host.ui.main)
    host.ui.add(host.ui.reviewPanel)
    host.ui.add(host.ui.outputViewer)
    host.ui.add(host.ui.interactionPanel)
    host.ui.add(host.ui.agentsStrip)
    host.ui.add(host.ui.composer)
    host.ui.add(host.ui.statusLine)
    host.ui.add(host.pickerContent.mountSlashPopup(host.context, theme))
    host.ui.add(host.ui.picker)
    host.ui.add(host.ui.commandPalette)
    host.ui.add(host.ui.mcpBrowser)
    host.ui.add(host.ui.settingsBrowser)
    host.ui.add(host.ui.themeBrowser)
    host.ui.add(host.ui.agentsBrowser)
    host.ui.add(host.ui.skillsBrowser)
}
