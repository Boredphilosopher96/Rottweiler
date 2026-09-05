import {
  BoxRenderable,
  SelectRenderable,
  SelectRenderableEvents,
  TextRenderable,
  bold,
  fg,
  t,
  type RenderContext
} from "@opentui/core"
import {
  formatStatusContext,
  formatStatusSessionCost
} from "../render"
import type { RottweilerState } from "../state"
import type { RottweilerTheme } from "../theme"

export interface ContextPanelCallbacks {
  readonly onOpenDiff?: (path: string) => void
  readonly onOpenSubagent?: (subagentId: string) => void
}

const MAX_SIDEBAR_CHANGED_FILES = 128

function contextPanelInputs(state: RottweilerState) {
  return [state.subagentOrder, state.subagents, state.todos, state.mcpServers,
  state.runtimeServices, state.review, state.workspaceStatus, state.context, state.cost, state.provider]
}

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
  #previousInputs: ReturnType<typeof contextPanelInputs> | null = null

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
    const inputs = contextPanelInputs(state)
    const previous = this.#previousInputs
    if (previous !== null && inputs.every((value, index) => value === previous[index])) return
    this.#previousInputs = inputs
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
