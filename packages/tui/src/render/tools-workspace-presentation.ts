import type { Cost, Usage } from "../protocol"
import type {
  ActivityTimingProjection,
  RottweilerState,
  ToolProjection,
} from "../state"
import { presentTool } from "./tool-presentation"
import { toolPlainText, toolStructuredData } from "./format"
import { truncateToCells } from "./text"

const OUTPUT_WINDOW_LINES = 8
const LIVE_OUTPUT_TRUNCATION_MARKER = "[live tool output truncated; command output continues to drain]"

export type ElapsedPresentation =
  | { readonly kind: "known"; readonly milliseconds: number; readonly label: string }
  | { readonly kind: "unknown"; readonly label: "-" }

export type ToolOutcomePresentation =
  | { readonly kind: "running"; readonly label: "live" }
  | { readonly kind: "awaiting_approval"; readonly label: "approval needed" }
  | { readonly kind: "succeeded"; readonly label: string }
  | {
      readonly kind: "denied"
      readonly label: "denied"
      readonly reason: "Permission denied. The tool was not run."
    }
  | { readonly kind: "failed"; readonly label: string }

export type ActivityOutputPresentation =
  | { readonly kind: "none" }
  | {
      readonly kind: "text"
      readonly text: string
      readonly retainedLineCount: number
      readonly visibleLineCount: number
      readonly hiddenRetainedLineCount: number
      readonly window: "head" | "tail"
      readonly sourceTruncated: boolean
    }

export interface ToolActivityPresentation {
  readonly kind: "tool"
  readonly key: `tool:${string}`
  readonly toolCallId: string
  readonly name: string
  readonly subject: string
  readonly outcome: ToolOutcomePresentation
  readonly elapsed: ElapsedPresentation
  readonly output: ActivityOutputPresentation
  readonly defaultExpanded: boolean
  readonly canOpenRetainedOutput: boolean
}

export interface ShellActivityPresentation {
  readonly kind: "foreground_shell"
  readonly key: `shell:${string}`
  readonly shellId: string
  readonly command: string
  readonly active: boolean
  readonly status: number | null
  readonly output: ActivityOutputPresentation
}

export type ActivityPresentation = ToolActivityPresentation | ShellActivityPresentation

export type TurnSummaryPresentation =
  | { readonly kind: "none" }
  | {
      readonly kind: "running"
      readonly turnId: string
      readonly toolCount: number
      readonly liveCount: number
      readonly deniedCount: number
      readonly elapsed: ElapsedPresentation
      readonly usage: null
      readonly cost: null
    }
  | {
      readonly kind: "finished"
      readonly turnId: string
      readonly toolCount: number
      readonly liveCount: 0
      readonly deniedCount: number
      readonly elapsed: ElapsedPresentation
      readonly usage: Usage
      readonly cost: Cost
    }

export interface QueuedMessagePresentation {
  readonly position: string
  readonly content: string
}

export interface ToolsWorkspacePresentation {
  readonly replay: boolean
  readonly rows: readonly ActivityPresentation[]
  readonly turn: TurnSummaryPresentation
  readonly queuedMessages: readonly QueuedMessagePresentation[]
}

export function projectToolsWorkspace(
  state: RottweilerState,
  nowMs: number,
): ToolsWorkspacePresentation {
  const turnId = selectedToolsTurnId(state)
  const tools = turnId === null
    ? []
    : Object.values(state.tools)
      .filter((tool) => tool.turnId === turnId)
      .sort((left, right) => left.callIndex - right.callIndex || left.toolCallId.localeCompare(right.toolCallId))
      .map((tool) => projectToolActivity(tool, nowMs, state.replay.active))
  let selectedShell: NonNullable<RottweilerState["transcript"][number]["shell"]> | null = null
  for (let index = state.transcript.length - 1; index >= 0; index -= 1) {
    const entry = state.transcript[index]
    const shell = entry?.presentation === "shell_result" ? entry.shell : undefined
    if (shell === undefined) continue
    if (state.shell.shellId !== null && shell.shellId !== state.shell.shellId) continue
    selectedShell = shell
    break
  }
  const shells: readonly ShellActivityPresentation[] = selectedShell === null
    ? []
    : [{
      kind: "foreground_shell",
      key: `shell:${selectedShell.shellId}`,
      shellId: selectedShell.shellId,
      command: selectedShell.command,
      active: selectedShell.active,
      status: selectedShell.status,
      output: outputWindow(
        selectedShell.capturedOutput,
        selectedShell.active ? "tail" : "head",
        selectedShell.outputTruncated,
      ),
    }]

  return {
    replay: state.replay.active,
    rows: [...tools, ...shells],
    turn: projectTurnSummary(state, turnId, nowMs),
    queuedMessages: state.queuedMessages.map(({ position, content }) => ({ position, content })),
  }
}

export function projectToolActivity(
  tool: ToolProjection,
  nowMs: number,
  replay: boolean,
): ToolActivityPresentation {
  const presentation = presentTool(tool)
  const outcome = toolOutcome(tool, presentation.summary)
  const liveOutput = tool.chunks.map((chunk) => chunk.chunk).join("")
  const outputText = tool.status === "finished" ? presentation.details : liveOutput
  const sourceTruncated = outputText.includes(LIVE_OUTPUT_TRUNCATION_MARKER) || hasDroppedOutput(tool)
  const output = outputWindow(outputText, tool.status === "finished" ? "head" : "tail", sourceTruncated)
  const fallbackSubject = argumentSubject(tool.args)

  return {
    kind: "tool",
    key: `tool:${tool.toolCallId}`,
    toolCallId: tool.toolCallId,
    name: tool.name,
    subject: truncateToCells((presentation.subject || fallbackSubject).replace(/\s+/g, " ").trim(), 80),
    outcome,
    elapsed: projectElapsed(tool.timing, nowMs, replay),
    output,
    defaultExpanded:
      tool.status === "running" ||
      tool.status === "awaiting_approval" ||
      outcome.kind === "denied",
    canOpenRetainedOutput: output.kind === "text",
  }
}

export function projectTurnSummary(
  state: RottweilerState,
  turnId: string | null,
  nowMs: number,
): TurnSummaryPresentation {
  if (turnId === null) return { kind: "none" }
  const turn = state.turns[turnId]
  if (turn === undefined) return { kind: "none" }
  const tools = Object.values(state.tools).filter((tool) => tool.turnId === turnId)
  const deniedCount = tools.filter(isPermissionDenied).length
  const elapsed = projectElapsed(turn.timing, nowMs, state.replay.active)

  if (turn.status === "running") {
    return {
      kind: "running",
      turnId,
      toolCount: tools.length,
      liveCount: tools.filter((tool) => tool.status === "running").length,
      deniedCount,
      elapsed,
      usage: null,
      cost: null,
    }
  }
  if (turn.usage === null || turn.cost === null) return { kind: "none" }
  return {
    kind: "finished",
    turnId,
    toolCount: tools.length,
    liveCount: 0,
    deniedCount,
    elapsed,
    usage: turn.usage,
    cost: turn.cost,
  }
}

export function selectedToolsTurnId(state: RottweilerState): string | null {
  if (state.streamingTail !== null) return state.streamingTail.turnId
  return Object.values(state.turns).at(-1)?.turnId ?? null
}

function toolOutcome(tool: ToolProjection, summary: string): ToolOutcomePresentation {
  if (tool.status === "running") return { kind: "running", label: "live" }
  if (tool.status === "awaiting_approval") {
    return { kind: "awaiting_approval", label: "approval needed" }
  }
  if (isPermissionDenied(tool)) {
    return {
      kind: "denied",
      label: "denied",
      reason: "Permission denied. The tool was not run.",
    }
  }
  const label = summary.replace(/\s+/g, " ").trim() || (tool.isError === true ? "failed" : "complete")
  return tool.isError === true
    ? { kind: "failed", label }
    : { kind: "succeeded", label }
}

function isPermissionDenied(tool: ToolProjection): boolean {
  return tool.isError === true && /^permission denied for tool/i.test(toolPlainText(tool.output).trim())
}

function outputWindow(
  text: string,
  window: "head" | "tail",
  sourceTruncated: boolean,
): ActivityOutputPresentation {
  const lines = text.replaceAll("\r\n", "\n").replaceAll("\r", "\n").split("\n")
  while (lines.at(-1) === "") lines.pop()
  if (lines.length === 0) return { kind: "none" }
  const visibleLines = window === "head"
    ? lines.slice(0, OUTPUT_WINDOW_LINES)
    : lines.slice(-OUTPUT_WINDOW_LINES)
  return {
    kind: "text",
    text: visibleLines.join("\n"),
    retainedLineCount: lines.length,
    visibleLineCount: visibleLines.length,
    hiddenRetainedLineCount: Math.max(0, lines.length - visibleLines.length),
    window,
    sourceTruncated,
  }
}

function projectElapsed(
  timing: ActivityTimingProjection,
  nowMs: number,
  replay: boolean,
): ElapsedPresentation {
  if (timing.kind === "unknown") return { kind: "unknown", label: "-" }
  if (timing.kind === "open") {
    const milliseconds = Math.max(
      0,
      (replay ? timing.lastObservedAtMs : nowMs) - timing.startedAtMs,
    )
    return { kind: "known", milliseconds, label: elapsedLabel(milliseconds) }
  }
  if (timing.startedAtMs === null) return { kind: "unknown", label: "-" }
  const milliseconds = Math.max(0, timing.finishedAtMs - timing.startedAtMs)
  return { kind: "known", milliseconds, label: elapsedLabel(milliseconds) }
}

function elapsedLabel(milliseconds: number): string {
  const totalSeconds = Math.floor(milliseconds / 1_000)
  const seconds = totalSeconds % 60
  const totalMinutes = Math.floor(totalSeconds / 60)
  if (totalMinutes < 60) {
    return `${totalMinutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`
  }
  const hours = Math.floor(totalMinutes / 60)
  const minutes = totalMinutes % 60
  return `${hours}:${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`
}

function argumentSubject(args: unknown): string {
  if (!isRecord(args)) return ""
  for (const key of ["command", "path", "file_path", "pattern", "query"] as const) {
    const value = args[key]
    if (typeof value === "string" && value.trim() !== "") return value
  }
  return ""
}

function hasDroppedOutput(tool: ToolProjection): boolean {
  const data = toolStructuredData(tool.output)
  return isRecord(data) && positiveNumber(data.dropped_output_bytes)
}

function positiveNumber(value: unknown): boolean {
  if (typeof value === "number") return Number.isFinite(value) && value > 0
  if (typeof value === "string") return /^\d+$/.test(value) && BigInt(value) > 0n
  return false
}

function isRecord(value: unknown): value is Readonly<Record<string, unknown>> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}
