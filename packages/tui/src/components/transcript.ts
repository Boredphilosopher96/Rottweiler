import {
  BoxRenderable,
  CodeRenderable,
  DiffRenderable,
  MarkdownRenderable,
  ScrollBoxRenderable,
  TextRenderable,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"

import {
  formatCost,
  filetypeForPath,
  formatToolArguments,
  getScrollAcceleration,
  presentableUnifiedDiff,
  terminalMarkdown,
  toolOutputText,
  turnMarkdown,
  turnReasoningMarkdown,
} from "../render"
import type {
  RottweilerState,
  SubagentProjection,
  ToolProjection,
  TranscriptEntry,
} from "../state"
import type { RottweilerTheme } from "../theme"

export interface TranscriptRenderableOptions {
  readonly syntaxStyle: SyntaxStyle
  readonly treeSitterClient?: TreeSitterClient
  readonly onInteraction?: () => void
}

const MAX_VISIBLE_SUBAGENTS = 8

function entryKey(entry: TranscriptEntry): string {
  return `${entry.sequenceId}:${entry.agentTurn}:${entry.turn.role}`
}

export class ReasoningBlockRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly body: MarkdownRenderable
  #content = ""
  #expanded = false
  #streaming = false
  #width = 80
  readonly #onExpansionChange: (expanded: boolean) => void
  readonly #onInteraction: (() => void) | undefined

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    options: {
      readonly content: string
      readonly expanded?: boolean
      readonly streaming?: boolean
      readonly width: number
      readonly treeSitterClient?: TreeSitterClient
      readonly onExpansionChange: (expanded: boolean) => void
      readonly onInteraction?: () => void
    },
  ) {
    super(ctx, {
      width: "100%",
      flexDirection: "column",
      flexShrink: 0,
      backgroundColor: theme.background,
      focusable: false,
    })
    this.#expanded = options.expanded ?? false
    this.#streaming = options.streaming ?? false
    this.#width = options.width
    this.#onExpansionChange = options.onExpansionChange
    this.#onInteraction = options.onInteraction
    this.header = new TextRenderable(ctx, {
      content: "",
      fg: theme.warning,
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
      selectable: true,
    })
    this.body = new MarkdownRenderable(ctx, {
      content: "",
      syntaxStyle,
      ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
      fg: colorWithOpacity(theme.markdownText, theme.thinkingOpacity),
      conceal: true,
      concealCode: false,
      streaming: this.#streaming,
      width: "100%",
      visible: this.#expanded,
      internalBlockMode: "top-level",
    })
    this.body.selectable = true
    this.header.onMouseDown = () => {
      this.toggle()
      this.#onInteraction?.()
    }
    this.add(this.header)
    this.add(this.body)
    this.update(options.content, this.#streaming, options.width)
  }

  get expanded(): boolean {
    return this.#expanded
  }

  update(content: string, streaming = this.#streaming, width = this.#width): void {
    this.#content = presentableReasoning(content)
    this.#streaming = streaming
    this.#width = width
    this.visible = this.#content !== ""
    this.body.streaming = streaming
    this.#layout()
  }

  collapse(notify = true): void {
    if (!this.#expanded) return
    this.#expanded = false
    this.#layout()
    if (notify) this.#onExpansionChange(false)
  }

  expand(notify = true): void {
    if (this.#expanded) return
    this.#expanded = true
    this.#layout()
    if (notify) this.#onExpansionChange(true)
  }

  toggle(): void {
    if (this.#content === "") return
    this.#expanded = !this.#expanded
    this.#layout()
    this.#onExpansionChange(this.#expanded)
  }

  #layout(): void {
    if (this.#content === "") {
      this.header.content = ""
      this.body.visible = false
      return
    }
    const state = this.#streaming ? "Thinking" : "Thought"
    const indicator = this.#streaming && !this.#expanded ? "◌" : this.#expanded ? "⌄" : "›"
    this.header.content = `${indicator} ${state}: ${reasoningTitle(this.#content)}`
    this.body.visible = this.#expanded
    this.body.content = this.#expanded ? this.#content : ""
  }
}

function presentableReasoning(content: string): string {
  return content.replaceAll("[REDACTED]", "").trim()
}

function reasoningTitle(content: string): string {
  const first = content
    .split("\n")
    .map((line) => line
      .replace(/\[([^\]]+)]\([^)]*\)/g, "$1")
      .replace(/[*_`~]/g, "")
      .replace(/^[\s#>-]+|[\s#>-]+$/g, "")
      .trim())
    .find(Boolean) ?? "Reasoning"
  return singleLine(first, 72)
}

function colorWithOpacity(color: string, opacity: number): string {
  const match = /^#([0-9A-Fa-f]{6})([0-9A-Fa-f]{2})?$/.exec(color)
  if (match === null) return color
  const sourceAlpha = match[2] === undefined ? 255 : Number.parseInt(match[2], 16)
  const alpha = Math.round(sourceAlpha * Math.max(0, Math.min(1, opacity)))
  return `#${match[1]}${alpha.toString(16).padStart(2, "0")}`
}

export class ToolBlockRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly body: TextRenderable
  command: CodeRenderable | TextRenderable | null = null
  diff: DiffRenderable | TextRenderable | null = null
  commandPrompt: TextRenderable | null = null
  #commandContainer: BoxRenderable | null = null
  #commandSignature = ""
  #diffSignature = ""
  #collapsed: boolean
  #tool: ToolProjection
  #theme: RottweilerTheme
  #onExpansionChange: ((expanded: boolean) => void) | undefined
  #rendering: TranscriptRenderableOptions | undefined

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    tool: ToolProjection,
    expanded?: boolean,
    onExpansionChange?: (expanded: boolean) => void,
    rendering?: TranscriptRenderableOptions,
  ) {
    super(ctx, {
      id: `tool-${tool.toolCallId}`,
      width: "100%",
      minHeight: 1,
      flexDirection: "column",
      border: false,
      backgroundColor: theme.background,
      // Expansion is mouse-driven while keyboard focus remains owned by the
      // transcript scroller/composer. Individual tool rows must not trap it.
      focusable: false,
      paddingX: 0,
      marginTop: 0,
    })
    this.#theme = theme
    this.#tool = tool
    this.#collapsed = expanded === undefined ? tool.status !== "awaiting_approval" : !expanded
    this.#onExpansionChange = onExpansionChange
    this.#rendering = rendering
    this.header = new TextRenderable(ctx, {
      content: "",
      fg: theme.foreground,
      height: 1,
      selectable: true,
    })
    this.body = new TextRenderable(ctx, {
      content: "",
      fg: theme.muted,
      wrapMode: "word",
      visible: !this.#collapsed,
      selectable: true,
    })
    this.onKeyDown = (key) => {
      if (key.name === "return" || key.name === "space") {
        key.preventDefault()
        this.toggle()
      }
    }
    // Collapse from the disclosure row only. Dragging across output or a diff
    // must remain a text-selection gesture instead of collapsing the card.
    this.header.onMouseDown = () => this.toggle()
    this.add(this.header)
    this.add(this.body)
    this.update(tool)
  }

  update(tool: ToolProjection): void {
    const previousStatus = this.#tool.status
    this.#tool = tool
    if (tool.status === "awaiting_approval" && previousStatus !== "awaiting_approval") {
      this.#collapsed = false
      this.body.visible = true
    }
    this.#syncCommand(tool)
    this.#syncDiff(tool)
    const glyph = tool.status === "awaiting_approval" ? "?" : tool.status === "running" ? "◌" : tool.isError === true ? "✕" : "✓"
    const approval = tool.status === "awaiting_approval" ? " · approval needed" : ""
    const args = this.command === null ? compactToolArguments(tool) : ""
    const result =
      tool.status === "finished" && this.#collapsed
        ? compactToolResult(tool)
        : ""
    this.header.content = `${this.#collapsed ? "›" : "⌄"} ${glyph} ${toolDisplayName(tool.name)}${args === "" ? "" : ` · ${args}`}${approval}${result === "" ? "" : ` · ${result}`}`
    this.header.fg =
      tool.status === "awaiting_approval"
        ? this.#theme.warning
        : tool.isError === true
          ? this.#theme.danger
          : tool.status === "finished"
            ? this.#theme.success
            : this.#theme.info
    if (this.#collapsed) {
      this.body.content = ""
      this.body.height = 0
      this.height = 1 + (this.#commandContainer?.height ?? 0) + (this.diff?.height ?? 0)
      return
    }
    this.body.content = boundedLines(toolBodyContent(tool), 8)
    const bodyRows = Math.min(8, Math.max(1, this.body.plainText.split("\n").length))
    this.body.height = bodyRows
    this.height = bodyRows + 1 + (this.#commandContainer?.height ?? 0) + (this.diff?.height ?? 0)
  }

  #syncCommand(tool: ToolProjection): void {
    const command = bashCommand(tool)
    const signature = command ?? ""
    if (signature === this.#commandSignature) return
    this.#commandSignature = signature
    if (this.#commandContainer !== null) {
      this.remove(this.#commandContainer)
      this.#commandContainer.destroyRecursively()
      this.#commandContainer = null
      this.command = null
      this.commandPrompt = null
    }
    if (command === null) return
    const content = visibleBashCommand(command)
    const rows = Math.max(1, content.split("\n").length)
    const container = new BoxRenderable(this.ctx, {
      id: `tool-command-row-${tool.toolCallId}`,
      width: "100%",
      height: rows,
      flexDirection: "row",
      flexShrink: 0,
    })
    this.commandPrompt = new TextRenderable(this.ctx, {
      content: bashPrompt(command),
      fg: this.#theme.muted,
      width: 2,
      height: rows,
      wrapMode: "none",
    })
    this.command = this.#rendering === undefined
      ? new TextRenderable(this.ctx, {
          content,
          fg: this.#theme.foreground,
          flexGrow: 1,
          height: rows,
          wrapMode: "none",
          selectable: true,
        })
      : new CodeRenderable(this.ctx, {
          id: `tool-command-${tool.toolCallId}`,
          flexGrow: 1,
          height: rows,
          content,
          filetype: "bash",
          syntaxStyle: this.#rendering.syntaxStyle,
          ...(this.#rendering.treeSitterClient === undefined
            ? {}
            : { treeSitterClient: this.#rendering.treeSitterClient }),
          drawUnstyledText: true,
          wrapMode: "none",
          streaming: false,
          selectable: true,
        })
    container.add(this.commandPrompt)
    container.add(this.command)
    this.#commandContainer = container
    this.insertBefore(container, this.diff ?? this.body)
  }

  #syncDiff(tool: ToolProjection): void {
    const proposal = readToolDiff(tool)
    const signature = proposal === null ? "" : `${proposal.path}\u0000${proposal.unifiedDiff}`
    if (signature === this.#diffSignature) return
    this.#diffSignature = signature
    if (this.diff !== null) {
      this.remove(this.diff)
      this.diff.destroyRecursively()
      this.diff = null
    }
    if (proposal === null) return
    const rows = Math.min(12, Math.max(4, proposal.unifiedDiff.split("\n").length))
    const filetype = filetypeForPath(proposal.path)
    this.diff = this.#rendering === undefined
      ? new TextRenderable(this.ctx, {
          content: boundedLines(proposal.unifiedDiff, rows),
          fg: this.#theme.foreground,
          height: rows,
          wrapMode: "none",
          selectable: true,
        })
      : new DiffRenderable(this.ctx, {
          id: `tool-diff-${tool.toolCallId}`,
          width: "100%",
          height: rows,
          diff: proposal.unifiedDiff,
          ...(filetype === undefined ? {} : { filetype }),
          syntaxStyle: this.#rendering.syntaxStyle,
          ...(this.#rendering.treeSitterClient === undefined
            ? {}
            : { treeSitterClient: this.#rendering.treeSitterClient }),
          view: "unified",
          wrapMode: "none",
          showLineNumbers: true,
          addedBg: this.#theme.added,
          removedBg: this.#theme.removed,
          contextBg: this.#theme.panel,
        })
    this.diff.selectable = true
    this.insertBefore(this.diff, this.body)
  }

  toggle(): void {
    this.#collapsed = !this.#collapsed
    this.body.visible = !this.#collapsed
    this.update(this.#tool)
    this.#onExpansionChange?.(!this.#collapsed)
  }
}

function toolBodyContent(tool: ToolProjection): string {
  const live = tool.chunks.map((chunk) => chunk.chunk).join("")
  const final = toolOutputText(tool.output)
  const rawOutput = tool.status === "finished" && final !== "" ? final : live
  const output = presentableToolText(rawOutput, tool.isError === true)
  const activity = tool.status === "awaiting_approval"
    ? "Awaiting approval…"
    : tool.status === "running"
      ? "Running…"
      : final === "" && live === ""
        ? "Completed with no output."
        : ""
  const argumentsLine = bashCommand(tool) === null ? detailedToolArguments(tool) : ""
  const rationale = tool.rationale === null || tool.rationale.trim() === ""
    ? ""
    : `Why · ${singleLine(tool.rationale, 240)}`
  const outputLabel = output === ""
    ? ""
    : `${tool.isError === true ? "Error" : tool.status === "running" ? "Live output" : "Result"}\n${output}`
  return [argumentsLine, rationale, outputLabel, activity]
    .filter(Boolean)
    .join("\n")
}

function presentableToolText(value: string, isError: boolean): string {
  const lines = value
    .replaceAll("\r\n", "\n")
    .replaceAll("\r", "\n")
    .split("\n")
    .map((line) => {
      if (/^error parsing diff:/i.test(line) || /line count did not match for hunk/i.test(line)) {
        return isError ? "Couldn't apply the requested change." : ""
      }
      return line
    })
    .filter((line, index, all) => line !== "" || (index > 0 && all[index - 1] !== ""))
  return lines.join("\n").trim()
}

function detailedToolArguments(tool: ToolProjection): string {
  if (!isRecord(tool.args)) {
    const summary = formatToolArguments(tool.args)
    return summary === "" ? "" : `Details · ${summary}`
  }
  const path = typeof tool.args.path === "string" ? tool.args.path : ""
  const pattern = typeof tool.args.pattern === "string" ? tool.args.pattern : ""
  const query = typeof tool.args.query === "string" ? tool.args.query : ""
  const url = typeof tool.args.url === "string" ? tool.args.url : ""
  const task = typeof tool.args.task === "string" ? tool.args.task : ""
  switch (tool.name) {
    case "read":
    case "write":
    case "edit":
      return path === "" ? "" : `File · ${path}`
    case "multi_edit":
    case "apply_worktree_diff":
      return path === "" ? "" : `Changes · ${path}`
    case "ls":
      return `Directory · ${path === "" ? "." : path}`
    case "glob":
      return `Pattern · ${pattern || "*"}${path === "" || path === "." ? "" : ` · in ${path}`}`
    case "grep":
    case "search":
      return `Search · ${query || pattern}${path === "" || path === "." ? "" : ` · in ${path}`}`
    case "webfetch":
      return url === "" ? "" : `URL · ${url}`
    case "websearch":
      return query === "" ? "" : `Search · ${query}`
    case "spawn_agent":
      return task === "" ? "" : `Task · ${singleLine(task, 180)}`
    default: {
      const summary = formatToolArguments(tool.args)
      return summary === "" ? "" : `Details · ${summary}`
    }
  }
}

function bashCommand(tool: ToolProjection): string | null {
  if ((tool.name !== "bash" && tool.name !== "shell") || !isRecord(tool.args)) return null
  return typeof tool.args.command === "string" ? tool.args.command : null
}

function visibleBashCommand(command: string): string {
  const lines = command.split("\n")
  const visible = lines.slice(0, 7)
  if (lines.length > visible.length) visible.push(`# … ${lines.length - visible.length} more lines`)
  return visible.join("\n")
}

function bashPrompt(command: string): string {
  const visibleRows = Math.min(7, command.split("\n").length)
  const prompts: string[] = Array.from({ length: visibleRows }, (_, index) => index === 0 ? "$" : ">")
  if (command.split("\n").length > visibleRows) prompts.push("·")
  return prompts.join("\n")
}

function boundedLines(value: string, maximum: number): string {
  const lines = value.split("\n")
  if (lines.length <= maximum) return value
  if (maximum <= 1) return `… ${lines.length} lines`
  return [...lines.slice(0, maximum - 1), `… ${lines.length - maximum + 1} more lines`].join("\n")
}

function readToolDiff(tool: ToolProjection): { path: string; unifiedDiff: string } | null {
  if (!isRecord(tool.diff)) return null
  return typeof tool.diff.path === "string" && typeof tool.diff.unified_diff === "string"
    ? {
        path: tool.diff.path,
        unifiedDiff: presentableUnifiedDiff(tool.diff.path, tool.diff.unified_diff),
      }
    : null
}

function toolBlockExpanded(
  tool: ToolProjection,
  expansion?: ReadonlyMap<string, boolean>,
): boolean {
  return expansion?.get(tool.toolCallId) ?? tool.status === "awaiting_approval"
}

function compactToolArguments(tool: ToolProjection): string {
  if (!isRecord(tool.args)) return formatToolArguments(tool.args, 64)
  const path = typeof tool.args.path === "string" ? tool.args.path : ""
  switch (tool.name) {
    case "ls":
      return path === "" ? "." : path
    case "glob": {
      const pattern = typeof tool.args.pattern === "string" ? tool.args.pattern : ""
      return `${pattern}${path === "" || path === "." ? "" : ` in ${path}`}`
    }
    case "read":
      return path
    case "bash":
    case "shell":
      return typeof tool.args.command === "string" ? singleLine(tool.args.command, 64) : ""
    default:
      return formatToolArguments(tool.args, 64)
  }
}

function compactToolResult(tool: ToolProjection): string {
  const result = toolOutputText(tool.output).trim()
  if (result === "") return tool.isError === true ? "Failed" : "Done"
  const lines = result.split("\n").filter(Boolean)
  if (tool.isError === true) return `Failed · ${singleLine(presentableToolText(result, true), 44)}`
  switch (tool.name) {
    case "read":
      return `${lines.length} line${lines.length === 1 ? "" : "s"}`
    case "glob":
    case "ls":
      return result === "No matching files." || result === "No entries."
        ? result
        : `${lines.length} item${lines.length === 1 ? "" : "s"}`
    case "grep":
    case "search":
      return result === "No matches." ? result : `${lines.length} match${lines.length === 1 ? "" : "es"}`
    case "write":
      return "File written"
    case "edit":
    case "multi_edit":
    case "apply_worktree_diff":
      return "Changes applied"
    case "todo":
      return "Todos updated"
    case "ask_user":
      return "Answered"
    case "submit_plan":
      return "Plan submitted"
    case "spawn_agent":
      return "Child started"
    case "background_kill":
      return "Process stopped"
    default:
      if (lines.length > 1) return `${lines.length} lines · ${singleLine(lines[0] ?? "", 32)}`
      return singleLine(result, 40)
  }
}

function toolDisplayName(name: string): string {
  return ({
    read: "Read file",
    write: "Write file",
    edit: "Edit file",
    multi_edit: "Edit files",
    grep: "Search text",
    search: "Search text",
    glob: "Find files",
    ls: "List directory",
    bash: "Terminal command",
    shell: "Terminal command",
    background_status: "Check background process",
    background_output: "Read background output",
    background_kill: "Stop background process",
    webfetch: "Fetch URL",
    websearch: "Search web",
    todo: "Update todos",
    ask_user: "Ask user",
    submit_plan: "Submit plan",
    symbols: "Find symbols",
    apply_worktree_diff: "Apply changes",
    tool_search: "Find tools",
    mcp_call: "MCP tool",
    spawn_agent: "Start child agent",
  } as Record<string, string>)[name] ?? name
    .replace(/^mcp__/, "MCP · ")
    .replaceAll("__", " · ")
    .replaceAll("_", " ")
    .replace(/^./, (letter) => letter.toUpperCase())
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

export class SubagentPanelRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly rows = new Map<string, TextRenderable>()
  readonly #theme: RottweilerTheme
  #rowOrder: readonly string[] = []

  constructor(ctx: RenderContext, theme: RottweilerTheme) {
    super(ctx, {
      id: "subagent-progress",
      width: "100%",
      height: 0,
      flexDirection: "column",
      flexShrink: 0,
      border: true,
      borderStyle: "single",
      borderColor: theme.border,
      backgroundColor: theme.panel,
      paddingX: 1,
      marginTop: 1,
      visible: false,
    })
    this.#theme = theme
    this.header = new TextRenderable(ctx, {
      content: "",
      fg: theme.accentStrong,
      height: 1,
      flexShrink: 0,
    })
    this.add(this.header)
  }

  update(subagents: readonly SubagentProjection[], total = subagents.length): void {
    const nextOrder = subagents.map((subagent) => subagent.projectionId)
    if (
      nextOrder.length !== this.#rowOrder.length ||
      nextOrder.some((subagentId, index) => subagentId !== this.#rowOrder[index])
    ) {
      for (const row of this.rows.values()) {
        this.remove(row)
        row.destroyRecursively()
      }
      this.rows.clear()
      this.#rowOrder = nextOrder
    }
    const currentIds = new Set(subagents.map((subagent) => subagent.projectionId))
    for (const [subagentId, row] of this.rows) {
      if (!currentIds.has(subagentId)) {
        this.remove(row)
        row.destroyRecursively()
        this.rows.delete(subagentId)
      }
    }

    const running = subagents.filter((subagent) => subagent.status === "running").length
    this.header.content = `Subagents · ${running} running · ${total} total`
    for (const [index, subagent] of subagents.entries()) {
      let row = this.rows.get(subagent.projectionId)
      if (row === undefined) {
        row = new TextRenderable(this.ctx, {
          content: "",
          fg: this.#theme.muted,
          height: 1,
          flexShrink: 0,
        })
        this.rows.set(subagent.projectionId, row)
        this.add(row)
      }
      const glyph = subagentGlyph(subagent.status)
      const branch = index === subagents.length - 1 ? "└─" : "├─"
      const detail = subagentDetail(subagent)
      row.content = `${branch} ${glyph} ${singleLine(subagent.task, 72)}${detail === "" ? "" : ` · ${detail}`}`
      row.fg =
        subagent.status === "failed"
          ? this.#theme.danger
          : subagent.status === "completed"
            ? this.#theme.success
            : subagent.status === "cancelled" ||
                subagent.status === "timed_out" ||
                subagent.status === "max_turns"
              ? this.#theme.warning
            : this.#theme.info
    }
    this.visible = subagents.length > 0
    this.height = subagents.length === 0 ? 0 : subagents.length + 3
  }
}

function subagentDetail(subagent: SubagentProjection): string {
  if (subagent.status === "running") {
    return subagent.activity ?? "starting"
  }
  const files = subagent.touchedFileCount === 0 ? "" : ` · ${subagent.touchedFileCount} files`
  const diff = subagent.diffArtifactId === null ? "" : " · diff ready"
  return `${subagent.summary === null ? subagent.status.replaceAll("_", " ") : singleLine(subagent.summary, 72)}${files}${diff}`
}

function subagentGlyph(status: SubagentProjection["status"]): string {
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

function singleLine(value: string, limit: number): string {
  const compact = value.replace(/\s+/g, " ").trim()
  return compact.length <= limit ? compact : `${compact.slice(0, Math.max(1, limit - 1))}…`
}

class TurnCardRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly markdown: MarkdownRenderable
  readonly reasoning: ReasoningBlockRenderable | null
  shellCommand: CodeRenderable | TextRenderable | null
  shellOutput: TextRenderable | null

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    entry: TranscriptEntry,
    width: number,
    detail: string | null,
    tools: readonly ToolProjection[],
    subagents: readonly SubagentProjection[],
    subagentTotal: number,
    toolExpansion: Map<string, boolean>,
    reasoningExpanded: boolean,
    onToolExpansionChange: (toolCallId: string, expanded: boolean) => void,
    onReasoningExpansionChange: (expanded: boolean) => void,
    onInteraction: (() => void) | undefined,
    treeSitterClient?: TreeSitterClient,
  ) {
    const shell = entry.presentation === "shell_result" ? entry.shell : undefined
    const markdown = shell === undefined
      ? terminalMarkdown(turnMarkdown(entry.turn), Math.max(20, width - 4))
      : ""
    const reasoning = shell === undefined ? turnReasoningMarkdown(entry.turn) : ""
    const toolOnly = entry.turn.role === "tool" && markdown === ""
    const role = shell !== undefined
      ? "Shell"
      : entry.presentation === "command_result"
      ? "Command result"
      : entry.turn.role === "assistant"
        ? "Rottweiler"
        : entry.turn.role === "user"
          ? "You"
          : "Tools"
    super(ctx, {
      id: `turn-${entryKey(entry)}`,
      width,
      flexDirection: "column",
      flexShrink: 0,
      border: shell !== undefined,
      ...(shell === undefined
        ? {}
        : {
            borderStyle: "single" as const,
            borderColor: shell.active
              ? theme.info
              : shell.status === 0
                ? theme.success
                : theme.danger,
          }),
      backgroundColor: shell !== undefined
        ? theme.panel
        : entry.turn.role === "user"
          ? theme.panelRaised
          : theme.background,
      paddingX: 1,
      paddingY: toolOnly ? 0 : 1,
      marginTop: shell === undefined ? 0 : 1,
    })
    this.header = new TextRenderable(ctx, {
      content: shell === undefined
        ? entry.presentation === "command_result"
          ? `${role} · ${entry.title ?? "completed"}`
          : `${role}${detail === null ? "" : ` · ${detail}`}`
        : shellHeader(shell.active, shell.status),
      fg: shell === undefined
        ? entry.turn.role === "assistant" ? theme.accentStrong : theme.info
        : shell.active
          ? theme.info
          : shell.status === 0
            ? theme.success
            : theme.danger,
      height: toolOnly ? 0 : 1,
      flexShrink: 0,
      visible: shell !== undefined || (!toolOnly && markdown !== ""),
      selectable: true,
    })
    this.markdown = new MarkdownRenderable(ctx, {
      id: `markdown-${entryKey(entry)}`,
      content: markdown,
      syntaxStyle,
      ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
      fg: theme.markdownText,
      conceal: true,
      concealCode: false,
      streaming: false,
      width: Math.max(1, width - 2),
      flexShrink: 0,
      visible: !toolOnly,
      internalBlockMode: "top-level",
      tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.markdown.selectable = true
    this.reasoning = reasoning === ""
      ? null
      : new ReasoningBlockRenderable(ctx, theme, syntaxStyle, {
          content: reasoning,
          expanded: reasoningExpanded,
          streaming: false,
          width,
          ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
          onExpansionChange: onReasoningExpansionChange,
          ...(onInteraction === undefined ? {} : { onInteraction }),
        })
    this.shellCommand = null
    this.shellOutput = null
    // Selection can focus a retained transcript node. The app restores its
    // configured keyboard-input target after the pointer interaction ends.
    this.onMouseUp = () => onInteraction?.()
    if (shell !== undefined) {
      this.add(this.header)
      const content = visibleBashCommand(shell.command)
      const rows = Math.max(1, content.split("\n").length)
      const commandRow = new BoxRenderable(ctx, {
        id: `shell-command-row-${shell.shellId}`,
        width: "100%",
        height: rows,
        flexDirection: "row",
        flexShrink: 0,
        marginTop: 1,
      })
      commandRow.add(new TextRenderable(ctx, {
        content: bashPrompt(shell.command),
        fg: theme.muted,
        width: 2,
        height: rows,
        wrapMode: "none",
      }))
      const renderedCommand = treeSitterClient === undefined
        ? new TextRenderable(ctx, {
            content,
            fg: theme.foreground,
            flexGrow: 1,
            height: rows,
            wrapMode: "none",
            selectable: true,
          })
        : new CodeRenderable(ctx, {
            id: `shell-command-${shell.shellId}`,
            flexGrow: 1,
            height: rows,
            content,
            filetype: "bash",
            syntaxStyle,
            treeSitterClient,
            drawUnstyledText: true,
            wrapMode: "none",
            streaming: false,
            selectable: true,
          })
      this.shellCommand = renderedCommand
      commandRow.add(renderedCommand)
      this.add(commandRow)
      const output = shell.capturedOutput.trimEnd()
      const renderedOutput = new TextRenderable(ctx, {
        id: `shell-output-${shell.shellId}`,
        content: output === ""
          ? shell.active ? "Running in the foreground terminal…" : "Completed with no output."
          : `Output${shell.outputTruncated ? " · truncated" : ""}\n${output}`,
        fg: output === "" ? theme.muted : theme.foreground,
        wrapMode: "word",
        flexShrink: 0,
        marginTop: 1,
        selectable: true,
      })
      this.shellOutput = renderedOutput
      this.add(renderedOutput)
      return
    }
    if (!toolOnly) {
      this.add(this.header)
      if (this.reasoning !== null) this.add(this.reasoning)
      this.add(this.markdown)
    }
    for (const tool of tools) {
      this.add(new ToolBlockRenderable(
        ctx,
        theme,
        tool,
        toolExpansion.get(tool.toolCallId),
        (expanded) => onToolExpansionChange(tool.toolCallId, expanded),
        {
          syntaxStyle,
          ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
        },
      ))
    }
    if (subagents.length > 0) {
      const panel = new SubagentPanelRenderable(ctx, theme)
      panel.update(subagents, subagentTotal)
      this.add(panel)
    }
  }
}

function shellHeader(active: boolean, status: number | null): string {
  if (active) return "◌ Shell · running"
  if (status === 0) return "✓ Shell · exited 0"
  if (status === null) return "■ Shell · finished"
  return `✕ Shell · exited ${status}`
}

export class TranscriptRenderable extends BoxRenderable {
  readonly scroller: ScrollBoxRenderable
  readonly streamingCard: BoxRenderable
  readonly streamingMarkdown: MarkdownRenderable
  readonly compactionCard: BoxRenderable
  readonly compactionMarkdown: MarkdownRenderable
  readonly subagentPanel: SubagentPanelRenderable
  readonly mountedCards = new Map<string, TurnCardRenderable>()
  readonly #tailReasoning: ReasoningBlockRenderable
  readonly #compactionReasoning: ReasoningBlockRenderable
  readonly #tailHeader: TextRenderable
  readonly #compactionHeader: TextRenderable
  readonly #tailCitations: TextRenderable
  readonly #tailTools: BoxRenderable
  readonly #theme: RottweilerTheme
  readonly #syntaxStyle: SyntaxStyle
  readonly #treeSitterClient: TreeSitterClient | undefined
  readonly #onInteraction: (() => void) | undefined
  readonly #toolExpansion = new Map<string, boolean>()
  readonly #tailToolCards = new Map<string, ToolBlockRenderable>()
  readonly #reasoningExpansion = new Map<string, boolean>()
  readonly #cardSignatures = new Map<string, string>()
  #state: RottweilerState | null = null
  #transcript: readonly TranscriptEntry[] | null = null
  #presentableTranscript: readonly TranscriptEntry[] = []
  #tools: RottweilerState["tools"] | null = null
  #turns: RottweilerState["turns"] | null = null
  #subagents: RottweilerState["subagents"] | null = null
  #tailReasoningTurnId: string | null = null
  #compactionAttempt: number | null = null

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    options: TranscriptRenderableOptions,
  ) {
    super(ctx, {
      id: "transcript",
      width: "100%",
      flexGrow: 1,
      minHeight: 1,
      backgroundColor: theme.background,
    })
    this.#theme = theme
    this.#syntaxStyle = options.syntaxStyle
    this.#treeSitterClient = options.treeSitterClient
    this.#onInteraction = options.onInteraction
    this.scroller = new ScrollBoxRenderable(ctx, {
      id: "transcript-scroll",
      width: "100%",
      height: "100%",
      scrollY: true,
      scrollX: false,
      stickyScroll: true,
      stickyStart: "bottom",
      scrollAcceleration: getScrollAcceleration(),
      viewportCulling: true,
      contentOptions: { flexDirection: "column", width: "100%" },
      verticalScrollbarOptions: { showArrows: false, trackOptions: { backgroundColor: theme.panel } },
    })
    this.scroller.onMouseUp = () => this.#onInteraction?.()
    this.streamingCard = new BoxRenderable(ctx, {
      id: "streaming-tail",
      width: "100%",
      minHeight: 2,
      flexDirection: "column",
      flexShrink: 0,
      backgroundColor: theme.background,
      paddingX: 1,
      paddingY: 1,
      visible: false,
    })
    this.#tailHeader = new TextRenderable(ctx, {
      content: "Rottweiler · streaming",
      fg: theme.accentStrong,
      height: 1,
      flexShrink: 0,
    })
    this.#tailReasoning = new ReasoningBlockRenderable(ctx, theme, options.syntaxStyle, {
      content: "",
      expanded: true,
      streaming: true,
      width: Math.max(20, this.width || ctx.width),
      ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
      onExpansionChange: () => {
        if (this.#state !== null) this.#updateTail(this.#state)
      },
      ...(this.#onInteraction === undefined ? {} : { onInteraction: this.#onInteraction }),
    })
    this.streamingMarkdown = new MarkdownRenderable(ctx, {
      id: "streaming-markdown",
      content: "",
      syntaxStyle: options.syntaxStyle,
      ...(options.treeSitterClient === undefined
        ? {}
        : { treeSitterClient: options.treeSitterClient }),
      fg: theme.markdownText,
      conceal: true,
      concealCode: false,
      streaming: true,
      width: "100%",
      flexShrink: 0,
      // OpenTUI's incremental parser can retain every completed top-level
      // block and only reconcile the unstable trailing block. Keeping one
      // persistent MarkdownRenderable in this mode prevents the raw-markdown
      // flash and full document relayout that otherwise occurs on each token.
      internalBlockMode: "top-level",
      tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.streamingMarkdown.selectable = true
    this.#tailCitations = new TextRenderable(ctx, {
      content: "",
      fg: theme.info,
      visible: false,
      wrapMode: "word",
      selectable: true,
    })
    this.#tailTools = new BoxRenderable(ctx, {
      id: "streaming-tools",
      width: "100%",
      flexDirection: "column",
    })
    this.subagentPanel = new SubagentPanelRenderable(ctx, theme)
    this.streamingCard.add(this.#tailHeader)
    this.streamingCard.add(this.#tailReasoning)
    this.streamingCard.add(this.streamingMarkdown)
    this.streamingCard.add(this.#tailCitations)
    this.streamingCard.add(this.subagentPanel)
    this.streamingCard.add(this.#tailTools)
    this.scroller.add(this.streamingCard)
    this.compactionCard = new BoxRenderable(ctx, {
      id: "compaction-stream",
      width: "100%",
      minHeight: 2,
      flexDirection: "column",
      flexShrink: 0,
      backgroundColor: theme.background,
      paddingX: 1,
      paddingY: 1,
      visible: false,
    })
    this.#compactionHeader = new TextRenderable(ctx, {
      content: "Rottweiler · compacting context",
      fg: theme.accentStrong,
      height: 1,
      flexShrink: 0,
    })
    this.#compactionReasoning = new ReasoningBlockRenderable(ctx, theme, options.syntaxStyle, {
      content: "",
      expanded: true,
      streaming: true,
      width: Math.max(20, this.width || ctx.width),
      ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
      onExpansionChange: () => {
        if (this.#state !== null) this.#updateCompaction(this.#state)
      },
      ...(this.#onInteraction === undefined ? {} : { onInteraction: this.#onInteraction }),
    })
    this.compactionMarkdown = new MarkdownRenderable(ctx, {
      id: "compaction-markdown",
      content: "",
      syntaxStyle: options.syntaxStyle,
      ...(options.treeSitterClient === undefined
        ? {}
        : { treeSitterClient: options.treeSitterClient }),
      fg: theme.markdownText,
      conceal: true,
      concealCode: false,
      streaming: true,
      width: "100%",
      flexShrink: 0,
      internalBlockMode: "top-level",
      tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.compactionMarkdown.selectable = true
    this.compactionCard.add(this.#compactionHeader)
    this.compactionCard.add(this.#compactionReasoning)
    this.compactionCard.add(this.compactionMarkdown)
    this.scroller.add(this.compactionCard)
    this.add(this.scroller)
  }

  get mountedEntryCount(): number {
    return this.mountedCards.size
  }

  get mountedKeys(): readonly string[] {
    return this.#presentableTranscript.map(entryKey)
  }

  update(state: RottweilerState): void {
    this.#state = state
    const transcriptChanged = this.#transcript !== state.transcript
    if (transcriptChanged && state.streamingTail === null && this.#tailReasoningTurnId !== null) {
      const committed = [...state.transcript]
        .reverse()
        .find((entry) =>
          entry.agentTurn === this.#tailReasoningTurnId && entry.turn.role === "assistant")
      if (committed !== undefined) {
        this.#reasoningExpansion.set(entryKey(committed), this.#tailReasoning.expanded)
      }
    }
    const historicalToolsChanged = this.#tools !== state.tools && toolProjectionChangedForHistory(
      this.#tools,
      state.tools,
      state.transcript,
      state.streamingTail?.turnId ?? null,
    )
    const cardProjectionChanged = historicalToolsChanged || this.#subagents !== state.subagents
    const turnProjectionChanged = this.#turns !== state.turns
    this.#transcript = state.transcript
    this.#tools = state.tools
    this.#turns = state.turns
    this.#subagents = state.subagents
    if (transcriptChanged || cardProjectionChanged) {
      this.#presentableTranscript = presentableTranscript(state)
    }
    this.#updateTail(state)
    this.#updateCompaction(state)
    if (transcriptChanged || cardProjectionChanged || turnProjectionChanged) {
      this.#reconcileHistory()
    }
  }

  setScrollOffset(scrollTop: number): void {
    this.scroller.scrollTo(scrollTop)
  }

  scrollBy(direction: 1 | -1, unit: "step" | "viewport"): void {
    this.scroller.scrollBy(direction, unit)
  }

  scrollTo(position: number): void {
    this.scroller.scrollTo(position)
  }

  protected override onResize(_width: number, _height: number): void {
    if (this.#state !== null) {
      this.#updateTail(this.#state)
      this.#updateCompaction(this.#state)
    }
    this.#reconcileHistory()
  }

  #reconcileHistory(): void {
    const state = this.#state
    if (state === null) return
    const width = Math.max(20, this.width || this.ctx.width)
    const transcript = this.#presentableTranscript
    const desiredKeys = new Set(transcript.map(entryKey))
    for (const [key, card] of this.mountedCards) {
      if (desiredKeys.has(key)) continue
      this.scroller.remove(card)
      card.destroyRecursively()
      this.mountedCards.delete(key)
      this.#cardSignatures.delete(key)
    }
    const toolEntryKeys = projectionEntryKeys(transcript, "tool")
    const subagentEntryKeys = projectionEntryKeys(transcript, "assistant")
    const lastAssistantEntryByTurn = new Map<string, string>()
    for (const transcriptEntry of transcript) {
      if (transcriptEntry.turn.role === "assistant") {
        lastAssistantEntryByTurn.set(transcriptEntry.agentTurn, entryKey(transcriptEntry))
      }
    }
    let reference: BoxRenderable = this.streamingCard
    for (let index = transcript.length - 1; index >= 0; index -= 1) {
      const entry = transcript[index]
      if (entry === undefined) continue
      const key = entryKey(entry)
      const tools = toolEntryKeys.has(key)
        ? Object.values(state.tools).filter((tool) => tool.turnId === entry.agentTurn)
        : []
      const turnSubagents = subagentEntryKeys.has(key)
        ? subagentsForTurn(state, entry.agentTurn)
        : []
      const visibleSubagents = boundedSubagents(turnSubagents)
      const detail = entry.turn.role === "assistant" &&
          lastAssistantEntryByTurn.get(entry.agentTurn) === key &&
          state.turns[entry.agentTurn]?.cost != null
        ? turnDetail(state.turns[entry.agentTurn]?.cost, state.turns[entry.agentTurn]?.usage)
        : null
      const signature = JSON.stringify([
        width,
        entry,
        detail,
        tools,
        visibleSubagents,
        turnSubagents.length,
        tools.map((tool) => this.#toolExpansion.get(tool.toolCallId) === true),
        this.#reasoningExpansion.get(key) === true,
      ])
      const retained = this.mountedCards.get(key)
      if (retained !== undefined && this.#cardSignatures.get(key) === signature) {
        reference = retained
        continue
      }
      if (retained !== undefined) {
        this.scroller.remove(retained)
        retained.destroyRecursively()
      }
      const card = new TurnCardRenderable(
        this.ctx,
        this.#theme,
        this.#syntaxStyle,
        entry,
        width,
        detail,
        tools,
        visibleSubagents,
        turnSubagents.length,
        this.#toolExpansion,
        this.#reasoningExpansion.get(key) ?? false,
        (toolCallId, expanded) => this.#rememberToolExpansion(toolCallId, expanded),
        (expanded) => this.#rememberReasoningExpansion(key, expanded),
        this.#onInteraction,
        this.#treeSitterClient,
      )
      this.scroller.insertBefore(card, reference)
      this.mountedCards.set(key, card)
      this.#cardSignatures.set(key, signature)
      reference = card
    }
  }

  #updateTail(state: RottweilerState): void {
    const tail = state.streamingTail
    const allSubagents =
      tail === null
        ? state.subagentOrder
            .map((subagentId) => state.subagents[subagentId])
            .filter(
              (subagent): subagent is SubagentProjection =>
                subagent !== undefined && subagent.status === "running",
            )
        : subagentsForTurn(state, tail.turnId)
    const subagents = boundedSubagents(allSubagents)
    this.subagentPanel.update(subagents, allSubagents.length)
    this.streamingCard.visible = tail !== null || subagents.length > 0
    if (tail === null) {
      // Finalize the trailing parser block before clearing it. This mirrors
      // OpenCode's streaming lifecycle and avoids leaving an unfinished code
      // fence/table parse state behind when the next response starts.
      this.streamingMarkdown.streaming = false
      this.streamingMarkdown.content = ""
      this.streamingMarkdown.visible = false
      this.#tailHeader.content = "Rottweiler · delegating"
      this.#tailReasoning.update("", false, Math.max(20, this.width || this.ctx.width))
      this.#tailReasoningTurnId = null
      this.#tailCitations.visible = false
      this.#replaceTailTools([])
      return
    }
    const tools = tail.toolCallIds
      .map((toolCallId) => state.tools[toolCallId])
      .filter((tool): tool is ToolProjection => tool !== undefined)
    const reasoning = presentableReasoning(tail.thinking)
    const activity = tools.some((tool) => tool.status === "awaiting_approval")
      ? "Waiting for approval"
      : tools.some((tool) => tool.status === "running")
        ? "Running tools"
        : reasoning !== ""
          ? "Thinking"
          : "Streaming"
    // Set the parser mode before appending content. Reversing this order makes
    // the new chunk take the non-streaming parse path for one frame, which is
    // the visible plain-text-then-Markdown flicker reported by users.
    this.streamingMarkdown.streaming = tail.finished === null
    this.streamingMarkdown.visible = tail.text.length > 0
    this.streamingMarkdown.content = terminalMarkdown(
      tail.text,
      Math.max(20, (this.width || this.ctx.width) - 4),
      tail.finished === null ? "streaming" : "complete",
    )
    this.#tailHeader.content = `Rottweiler · ${tail.finished === null ? activity : turnDetail(tail.finished.cost, tail.finished.usage)}`
    if (this.#tailReasoningTurnId !== tail.turnId) {
      this.#tailReasoning.expand(false)
      this.#tailReasoningTurnId = tail.turnId
    }
    this.#tailReasoning.update(
      reasoning,
      tail.finished === null,
      Math.max(20, this.width || this.ctx.width),
    )
    this.#tailCitations.visible = tail.citations.length > 0
    this.#tailCitations.content = tail.citations
      .map((citation, index) => `[${index + 1}] ${citation.title ?? citation.uri}`)
      .join("  ")
    this.#tailCitations.height = tail.citations.length > 0 ? 1 : 0
    this.#replaceTailTools(tools)
  }

  #updateCompaction(state: RottweilerState): void {
    const compaction = state.compaction
    this.compactionCard.visible = compaction.active &&
      (compaction.text !== "" || compaction.thinking !== "")
    if (!compaction.active) {
      this.compactionMarkdown.streaming = false
      this.compactionMarkdown.content = ""
      this.compactionMarkdown.visible = false
      this.#compactionReasoning.update("", false, Math.max(20, this.width || this.ctx.width))
      this.#compactionAttempt = null
      return
    }
    if (this.#compactionAttempt !== compaction.attempt) {
      this.#compactionReasoning.expand(false)
      this.#compactionAttempt = compaction.attempt
    }
    const width = Math.max(20, (this.width || this.ctx.width) - 4)
    this.compactionMarkdown.streaming = true
    this.compactionMarkdown.visible = compaction.text !== ""
    this.compactionMarkdown.content = terminalMarkdown(compaction.text, width, "streaming")
    this.#compactionReasoning.update(
      compaction.thinking,
      true,
      Math.max(20, this.width || this.ctx.width),
    )
    this.#compactionHeader.content = compaction.text === "" && compaction.thinking === ""
      ? "Rottweiler · compacting context"
      : "Rottweiler · summarizing context"
  }

  #replaceTailTools(tools: readonly ToolProjection[]): void {
    const retained = new Set(tools.map((tool) => tool.toolCallId))
    for (const [toolCallId, card] of this.#tailToolCards) {
      if (retained.has(toolCallId)) continue
      this.#tailTools.remove(card)
      card.destroyRecursively()
      this.#tailToolCards.delete(toolCallId)
    }
    for (const tool of tools) {
      let card = this.#tailToolCards.get(tool.toolCallId)
      if (card === undefined) {
        card = new ToolBlockRenderable(
          this.ctx,
          this.#theme,
          tool,
          this.#toolExpansion.get(tool.toolCallId),
          (expanded) => this.#rememberToolExpansion(tool.toolCallId, expanded),
          {
            syntaxStyle: this.#syntaxStyle,
            ...(this.#treeSitterClient === undefined
              ? {}
              : { treeSitterClient: this.#treeSitterClient }),
          },
        )
        this.#tailToolCards.set(tool.toolCallId, card)
        this.#tailTools.add(card)
      } else {
        card.update(tool)
      }
    }
  }

  #rememberToolExpansion(toolCallId: string, expanded: boolean): void {
    if (this.#toolExpansion.get(toolCallId) === expanded) return
    this.#toolExpansion.set(toolCallId, expanded)
    const state = this.#state
    if (state === null) return
    this.#updateTail(state)
  }

  #rememberReasoningExpansion(key: string, expanded: boolean): void {
    if ((this.#reasoningExpansion.get(key) ?? false) === expanded) return
    this.#reasoningExpansion.set(key, expanded)
  }
}

function toolProjectionChangedForHistory(
  previous: RottweilerState["tools"] | null,
  next: RottweilerState["tools"],
  transcript: readonly TranscriptEntry[],
  streamingTurnId: string | null,
): boolean {
  if (previous === null) return true
  const historicalTurns = new Set(
    transcript
      .map((entry) => entry.agentTurn)
      .filter((turnId) => turnId !== streamingTurnId),
  )
  const ids = new Set([...Object.keys(previous), ...Object.keys(next)])
  for (const id of ids) {
    const before = previous[id]
    const after = next[id]
    if (before === after) continue
    if (
      (before !== undefined && historicalTurns.has(before.turnId)) ||
      (after !== undefined && historicalTurns.has(after.turnId))
    ) return true
  }
  return false
}

function turnDetail(
  cost: Parameters<typeof formatCost>[0],
  usage: Parameters<typeof formatCost>[1],
): string {
  const detail = formatCost(cost, usage)
  return cost?.kind === "subscription_quota" && (cost.used === null || cost.used === undefined)
    ? `turn usage · ${detail}`
    : detail
}

function projectionEntryKeys(
  transcript: readonly TranscriptEntry[],
  preferredRole: TranscriptEntry["turn"]["role"],
): Set<string> {
  const fallback = new Map<string, TranscriptEntry>()
  const preferred = new Map<string, TranscriptEntry>()
  for (const entry of transcript) {
    fallback.set(entry.agentTurn, entry)
    if (entry.turn.role === preferredRole) preferred.set(entry.agentTurn, entry)
  }
  return new Set(
    [...fallback.keys()].map((turnId) => entryKey(preferred.get(turnId) ?? fallback.get(turnId)!)),
  )
}

/**
 * Provider continuation state is durable but not user-facing. Do not mount an
 * empty assistant card merely because a committed turn retained an encrypted
 * reasoning signature. Tool and subagent-only turns remain visible through
 * their dedicated projections.
 */
function presentableTranscript(state: RottweilerState): TranscriptEntry[] {
  const toolTurns = new Set(Object.values(state.tools).map((tool) => tool.turnId))
  const subagentTurns = new Set(
    Object.values(state.subagents).map((subagent) => subagent.parentTurnId),
  )
  return state.transcript.filter((entry) => {
    if (entry.presentation === "shell_result" && entry.shell !== undefined) return true
    if (turnMarkdown(entry.turn).trim() !== "") return true
    if (turnReasoningMarkdown(entry.turn) !== "") return true
    if (entry.turn.role === "tool" && toolTurns.has(entry.agentTurn)) return true
    if (entry.turn.role === "assistant" && subagentTurns.has(entry.agentTurn)) return true
    return false
  })
}

function subagentsForTurn(state: RottweilerState, turnId: string): SubagentProjection[] {
  return state.subagentOrder
    .map((subagentId) => state.subagents[subagentId])
    .filter(
      (subagent): subagent is SubagentProjection =>
        subagent !== undefined && subagent.parentTurnId === turnId,
    )
}

function boundedSubagents(subagents: readonly SubagentProjection[]): SubagentProjection[] {
  if (subagents.length <= MAX_VISIBLE_SUBAGENTS) return [...subagents]
  const running = subagents.filter((subagent) => subagent.status === "running")
  const completed = subagents.filter((subagent) => subagent.status !== "running")
  const visibleRunning = running.slice(0, MAX_VISIBLE_SUBAGENTS)
  const remaining = MAX_VISIBLE_SUBAGENTS - visibleRunning.length
  if (remaining === 0) return visibleRunning
  return [...visibleRunning, ...completed.slice(-remaining)]
}
