import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"

import { createCanvas } from "@napi-rs/canvas"
import { createTestRenderer } from "@opentui/core/testing"
import {
  type BaseRenderable,
  type CapturedFrame,
  CodeRenderable,
  TextAttributes,
  TreeSitterClient,
  getBaseAttributes,
} from "@opentui/core"

import { createRottweilerApp } from "../src/app"
import type { RottweilerState, ToolProjection } from "../src/state"
import { createInitialState } from "../src/state"
import { kennelTheme } from "../src/theme"

type VisualScenario = "conversation" | "command-palette" | "approval"

const scenarioInput = process.argv[2] ?? "conversation"
if (!isVisualScenario(scenarioInput)) {
  throw new Error(`Unknown scenario: ${scenarioInput}`)
}
const outputDirectory = resolve(process.argv[3] ?? `/tmp/rottweiler-tui-evidence/${scenarioInput}`)
const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
const parserDataPath = await mkdtemp(join(tmpdir(), "rottweiler-visual-"))
const treeSitter = new TreeSitterClient({
  dataPath: parserDataPath,
  workerPath: resolve(import.meta.dir, "../node_modules/@opentui/core/parser.worker.js"),
})
await treeSitter.initialize()

try {
  const app = createRottweilerApp(setup.renderer, {
    initialState: scenarioState(scenarioInput),
    requestId: () => "visual-proof-request",
    treeSitterClient: treeSitter,
  })
  setup.renderer.root.add(app)
  await settleHighlights(setup.renderer.root, setup)

  const actions: string[] = []
  if (scenarioInput === "command-palette") {
    actions.push("pressed Ctrl+P through the renderer input path")
    setup.mockInput.pressKey("p", { ctrl: true })
    actions.push("typed context into the focused production query input")
    await setup.mockInput.typeText("context")
    await setup.flush()
  } else if (scenarioInput === "approval") {
    actions.push("launched a session with a pending terminal-command approval")
  } else {
    actions.push("launched a live conversation with reasoning, tools, tasks, changed files, and child agents")
  }

  await mkdir(outputDirectory, { recursive: true })
  const characterFrame = setup.captureCharFrame()
  const styledFrame = setup.captureSpans()
  const assertions = [
    ...visualAssertions(scenarioInput, characterFrame, styledFrame),
    ...(scenarioInput === "command-palette" ? commandPaletteLayoutAssertions(app) : []),
  ]
  await writeRasterPng(styledFrame, join(outputDirectory, `${scenarioInput}.png`))
  await Promise.all([
    writeFile(join(outputDirectory, `${scenarioInput}.txt`), characterFrame),
    writeFile(join(outputDirectory, `${scenarioInput}.ansi`), frameAnsi(styledFrame)),
    writeFile(join(outputDirectory, `${scenarioInput}.json`), JSON.stringify({
      scenario: scenarioInput,
      terminal: { columns: styledFrame.cols, rows: styledFrame.rows },
      presentationContract: {
        ansi: "24-bit terminal-native text runs",
        png: "direct raster text runs with a system monospace font",
        characterSvg: false,
      },
      actions,
      assertions,
    }, null, 2) + "\n"),
  ])

  const failed = assertions.filter((assertion) => !assertion.passed)
  if (failed.length > 0) {
    throw new Error(`Visual proof failed: ${failed.map((assertion) => assertion.name).join(", ")}`)
  }
  process.stdout.write(`${outputDirectory}\n`)
} finally {
  setup.renderer.destroy()
  await treeSitter.destroy()
  await rm(parserDataPath, { recursive: true, force: true })
}

function isVisualScenario(value: string): value is VisualScenario {
  return value === "conversation" || value === "command-palette" || value === "approval"
}

function scenarioAssertions(scenario: VisualScenario): readonly string[] {
  switch (scenario) {
    case "conversation":
      return ["you", "● rottweiler", "reasoning", "AGENTS", "TASKS", "CHANGED", "SESSION"]
    case "command-palette":
      return ["COMMAND PALETTE", "context", "Compact context", "Manage context"]
    case "approval":
      return ["Permission required", "Terminal command", "Allow once"]
  }
}

interface VisualAssertion {
  readonly name: string
  readonly passed: boolean
  readonly expected: string
  readonly actual: string
}

function visualAssertions(
  scenario: VisualScenario,
  characterFrame: string,
  styledFrame: CapturedFrame,
): VisualAssertion[] {
  const assertions = scenarioAssertions(scenario).map((text): VisualAssertion => ({
    name: `visible text: ${text}`,
    passed: characterFrame.includes(text),
    expected: text,
    actual: characterFrame.includes(text) ? text : "missing",
  }))
  if (scenario === "command-palette") {
    const lines = characterFrame.split("\n")
    return [
      ...assertions,
      positionAssertion(lines, "query starts at the design column", 3, 3, "context"),
      positionAssertion(lines, "list/detail divider is fixed at column 55", 5, 55, "│"),
      positionAssertion(lines, "filtered count and source counts are derived", 25, 3, "4 of 30 commands · 30 built-in · 0 extensions"),
      frameWidthAssertion(lines),
      {
        name: "selected description appears only in detail",
        passed: occurrenceCount(characterFrame, "Inspect assembled context") === 1,
        expected: "1 occurrence",
        actual: `${occurrenceCount(characterFrame, "Inspect assembled context")} occurrences`,
      },
      colorAssertion(styledFrame, "query uses normal text", 3, 3, kennelTheme.text),
      colorAssertion(styledFrame, "selection marker uses primary", 5, 3, kennelTheme.primary, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "unmatched title text stays readable", 5, 5, kennelTheme.text, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "matched title text uses primary", 5, 6, kennelTheme.primary, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "divider uses subtle border", 5, 55, kennelTheme.borderSubtle),
      colorAssertion(styledFrame, "detail metadata is muted", 6, 57, kennelTheme.textMuted),
    ]
  }
  if (scenario !== "conversation") return assertions

  const lines = characterFrame.split("\n")
  const positioned = [
    positionAssertion(lines, "user gutter begins at column 0", 0, 0, "▌ you"),
    positionAssertion(lines, "assistant marker begins at column 0", 4, 0, "● rottweiler"),
    positionAssertion(lines, "reasoning rail begins at column 0", 6, 0, "╎ reasoning"),
    positionAssertion(lines, "assistant prose uses two-cell indent", 12, 2, "What changed"),
    positionAssertion(lines, "context divider is fixed at column 73", 0, 73, "│"),
    positionAssertion(lines, "composer is inset one column", 27, 1, "╭"),
    positionAssertion(lines, "status is inset one column", 31, 1, " EXECUTE "),
    positionAssertion(lines, "context rows align with their headings", 1, 75, "◌"),
    positionAssertion(lines, "tool rows use the two-cell assistant indent", 21, 2, "▸ edit"),
    positionAssertion(lines, "agents heading and count are not truncated", 0, 73, "│ AGENTS                   2 running "),
    positionAssertion(lines, "agent activity keeps its right padding", 1, 73, "│ ◌ explore  reading transport code  "),
    positionAssertion(lines, "tasks heading and count are not truncated", 4, 73, "│ TASKS                          1/3 "),
    positionAssertion(lines, "changed heading and count are not truncated", 9, 73, "│ CHANGED                          3 "),
    positionAssertion(lines, "session values are not truncated", 15, 73, "│ ctx    13k/32k (41%)               "),
    positionAssertion(lines, "service names are not truncated", 20, 73, "│ LSP · rust-analyzer                "),
    frameWidthAssertion(lines),
    {
      name: "production Markdown is concealed",
      passed: !characterFrame.includes("## ") &&
        !characterFrame.includes("**durable**") &&
        !characterFrame.includes("`cursor.rs`"),
      expected: "no visible Markdown control characters",
      actual: characterFrame.includes("## ") || characterFrame.includes("**durable**")
        ? "raw Markdown is visible"
        : "concealed",
    },
  ]
  const colors = [
    colorAssertion(styledFrame, "user gutter uses primary", 0, 0, kennelTheme.primary),
    colorAssertion(styledFrame, "assistant marker uses accent", 4, 0, kennelTheme.accent),
    colorAssertion(styledFrame, "reasoning label is muted", 6, 2, kennelTheme.textMuted),
    colorAssertion(styledFrame, "reasoning prose stays muted", 7, 2, kennelTheme.textMuted),
    colorAssertion(styledFrame, "reasoning line 2 stays muted", 8, 2, kennelTheme.textMuted),
    colorAssertion(styledFrame, "reasoning line 3 stays muted", 9, 2, kennelTheme.textMuted),
    colorAssertion(styledFrame, "reasoning line 4 stays muted", 10, 2, kennelTheme.textMuted),
    colorAssertion(styledFrame, "context heading uses info", 0, 75, kennelTheme.info),
    colorAssertion(styledFrame, "tool name uses secondary", 21, 4, kennelTheme.secondary),
    colorAssertion(styledFrame, "tool outcome uses success", 21, 62, kennelTheme.success),
    colorAssertion(styledFrame, "mode pill uses primary background", 31, 2, kennelTheme.background, kennelTheme.primary),
  ]
  return [...assertions, ...positioned, ...colors]
}

function commandPaletteLayoutAssertions(
  app: ReturnType<typeof createRottweilerApp>,
): readonly VisualAssertion[] {
  const palette = app.commandPalette
  return [
    exactValueAssertion("modal begins at column 1", palette.x, 1),
    exactValueAssertion("modal begins at row 2", palette.y, 2),
    exactValueAssertion("modal is 108 cells wide", palette.width, 108),
    exactValueAssertion("modal is 25 rows tall", palette.height, 25),
    exactValueAssertion("list pane is 52 cells wide", palette.listPane.width, 52),
    exactValueAssertion("divider is one cell wide", palette.divider.width, 1),
    exactValueAssertion("detail pane is 51 cells wide", palette.detailPane.width, 51),
    {
      name: "query value remains intact",
      passed: palette.input.value === "context",
      expected: "context",
      actual: palette.input.value,
    },
  ]
}

function exactValueAssertion(name: string, actual: number, expected: number): VisualAssertion {
  return {
    name,
    passed: actual === expected,
    expected: String(expected),
    actual: String(actual),
  }
}

function occurrenceCount(value: string, needle: string): number {
  return value.split(needle).length - 1
}

function frameWidthAssertion(lines: readonly string[]): VisualAssertion {
  const widths = lines.slice(0, 32).map((line) => Bun.stringWidth(line))
  const actual = `${widths.length} rows; widths ${Math.min(...widths)}-${Math.max(...widths)}`
  return {
    name: "every terminal row retains the complete 110-cell frame",
    passed: widths.length === 32 && widths.every((width) => width === 110),
    expected: "32 rows; widths 110-110",
    actual,
  }
}

function positionAssertion(
  lines: readonly string[],
  name: string,
  row: number,
  column: number,
  expected: string,
): VisualAssertion {
  const actual = lines[row]?.slice(column, column + expected.length) ?? ""
  return { name, passed: actual === expected, expected, actual }
}

function colorAssertion(
  frame: CapturedFrame,
  name: string,
  row: number,
  column: number,
  expectedForeground: string,
  expectedBackground?: string,
): VisualAssertion {
  const cell = capturedCell(frame, row, column)
  const foreground = cell === null ? "missing" : rgbaHex(cell.fg.toInts())
  const background = cell === null ? "missing" : rgbaHex(cell.bg.toInts())
  const expected = expectedBackground === undefined
    ? normalizeHex(expectedForeground)
    : `${normalizeHex(expectedForeground)} on ${normalizeHex(expectedBackground)}`
  const actual = expectedBackground === undefined ? foreground : `${foreground} on ${background}`
  return {
    name,
    passed: foreground === normalizeHex(expectedForeground) &&
      (expectedBackground === undefined || background === normalizeHex(expectedBackground)),
    expected,
    actual,
  }
}

function capturedCell(frame: CapturedFrame, row: number, column: number) {
  let start = 0
  for (const span of frame.lines[row]?.spans ?? []) {
    if (column >= start && column < start + span.width) return span
    start += span.width
  }
  return null
}

function rgbaHex(channels: readonly number[]): string {
  const [red = 0, green = 0, blue = 0, alpha = 255] = channels
  return `${rgbHex(red, green, blue)}${alpha === 255 ? "" : alpha.toString(16).padStart(2, "0")}`.toUpperCase()
}

function normalizeHex(color: string): string {
  return color.toUpperCase()
}

async function settleHighlights(
  root: BaseRenderable,
  renderer: Awaited<ReturnType<typeof createTestRenderer>>,
): Promise<void> {
  for (let attempt = 0; attempt < 30; attempt += 1) {
    await renderer.flush()
    const pending = new Set<CodeRenderable>()
    const visit = (node: BaseRenderable): void => {
      if (node instanceof CodeRenderable && node.isHighlighting) pending.add(node)
      for (const child of node.getChildren()) visit(child)
    }
    visit(root)
    if (pending.size === 0) {
      await renderer.waitForVisualIdle()
      return
    }
    await Promise.all([...pending].map((renderable) => renderable.highlightingDone))
  }
  throw new Error("Timed out waiting for the production Markdown renderer")
}

function scenarioState(scenario: VisualScenario): RottweilerState {
  const state = conversationState()
  if (scenario !== "approval") return state
  const approval = tool({
    toolCallId: "approval-tool",
    name: "bash",
    args: { command: "cargo test -p rw-core" },
    status: "awaiting_approval",
    capabilities: ["execute"],
    rationale: "Run the focused reconnect regression suite",
    output: null,
    chunks: [],
    isError: null,
  })
  return {
    ...state,
    streamingTail: {
      turnId: "2",
      text: "I need permission before running the focused regression suite.",
      thinking: "The command executes workspace code, so it must cross the approval boundary.",
      citations: [],
      toolCallIds: [approval.toolCallId],
      finished: null,
    },
    tools: { [approval.toolCallId]: approval },
  }
}

function conversationState(): RottweilerState {
  const initial = createInitialState()
  const edit = tool({
    toolCallId: "edit-tool",
    name: "edit",
    args: { path: "core/cursor.rs" },
    status: "finished",
    capabilities: ["write_filesystem"],
    rationale: "Track the durable cursor independently",
    output: { type: "text", text: "Updated core/cursor.rs" },
    chunks: [],
    isError: false,
  })
  const tests = tool({
    toolCallId: "test-tool",
    name: "bash",
    args: { command: "cargo test -p rw-core" },
    status: "finished",
    capabilities: ["execute"],
    rationale: "Run the focused regression suite",
    output: { type: "text", text: "18 passed; 0 failed" },
    chunks: [],
    isError: false,
  })
  const read = tool({
    toolCallId: "read-tool",
    name: "read",
    args: { path: "protocol/session-log.md" },
    status: "finished",
    capabilities: ["read_filesystem"],
    rationale: "Confirm the reconnect contract",
    output: { type: "text", text: "184 lines" },
    chunks: [],
    isError: false,
  })
  return {
    ...initial,
    connection: { phase: "connected", attempt: 0, error: null, gap: null },
    mode: "execute",
    provider: "anthropic",
    model: "sonnet-4.5",
    transcript: [{
      sequenceId: "1",
      agentTurn: "1",
      turn: {
        role: "user",
        blocks: [{
          type: "text",
          text: "Add reconnect-safe streaming. The cursor double-advances after a dropped SSE connection.",
        }],
        meta: { synthetic: false, summary: false },
      },
    }],
    streamingTail: {
      turnId: "2",
      text: "## What changed\n\nThe stream resumes from the last **durable** sequence, not the last delivered frame.\n\n1. `cursor.rs` tracks `durable_seq` independently\n2. `sse.ts` replays from that sequence on reattach\n3. `app.ts` drops the transport-ack fast path",
      thinking: "Two acknowledgements exist here: the transport ack and\nthe durable sequence ack. The client advances its cursor\non the transport ack, so a reconnect replays from a\nsequence the UI already consumed. Keep them separate.",
      citations: [{ uri: "protocol/session-log.md", title: "Reconnect contract" }],
      toolCallIds: [edit.toolCallId, tests.toolCallId, read.toolCallId],
      finished: null,
    },
    tools: {
      [edit.toolCallId]: edit,
      [tests.toolCallId]: tests,
      [read.toolCallId]: read,
    },
    todos: [
      { id: "map", content: "Map the event stream", status: "completed" },
      { id: "cursor", content: "Add durable cursor", status: "in_progress" },
      { id: "tests", content: "Test reconnect replay", status: "pending" },
    ],
    subagentOrder: ["explore", "tests"],
    subagents: {
      explore: {
        projectionId: "explore",
        subagentId: "explore",
        parentTurnId: "2",
        task: "Map the reconnect path",
        spawnedAtMs: null,
        status: "running",
        childSessionId: "child-explore",
        lastChildSequence: "4",
        activity: "reading transport code",
        summary: null,
        touchedFileCount: 0,
        diffArtifactId: null,
      },
      tests: {
        projectionId: "tests",
        subagentId: "tests",
        parentTurnId: "2",
        task: "Check replay regressions",
        spawnedAtMs: null,
        status: "running",
        childSessionId: "child-tests",
        lastChildSequence: "3",
        activity: "running focused tests",
        summary: null,
        touchedFileCount: 0,
        diffArtifactId: null,
      },
    },
    workspaceStatus: {
      workspaceName: "Rottweiler",
      branch: "feat/tui-v2",
      changedPaths: ["core/cursor.rs", "tui/transport/sse.ts", "core/durable.rs"],
      truncated: false,
    },
    runtimeServices: [{ kind: "lsp", name: "rust-analyzer", status: "ready" }],
    context: {
      turn_id: "2",
      stable_prefix_hash: "visual-proof",
      used_tokens: "13200",
      usable_tokens: "32000",
      reserved_tokens: "4000",
      context_window_known: true,
      cache_breakpoints: [{ after_item_id: "policy" }],
      items: [],
    },
    cost: costSnapshot(),
    commands: [
      { name: "context", description: "Inspect assembled context", usage: "/context" },
      { name: "review", description: "Review cumulative changes", usage: "/review" },
      { name: "sessions", description: "Search and resume sessions", usage: "/sessions" },
    ],
  }
}

function tool(fields: Omit<ToolProjection, "turnId" | "diff" | "callIndex">): ToolProjection {
  return {
    ...fields,
    turnId: "2",
    diff: null,
    callIndex: 0,
  }
}

function costSnapshot(): NonNullable<RottweilerState["cost"]> {
  const usage = {
    input_tokens: "12000",
    output_tokens: "1200",
    cache_read_tokens: "9000",
    cache_write_tokens: "0",
    reasoning_tokens: "512",
  }
  return {
    utc_day: "2026-08-25",
    turns: [],
    session_usage: usage,
    session_cost_micros_usd: "412000",
    session_ai_credit_micros: "0",
    session_subscription_tokens: "0",
    daily_cost_micros_usd: "412000",
    daily_ai_credit_micros: "0",
    daily_subscription_tokens: "0",
    trailing_minute_cost_micros_usd: "21000",
    trailing_minute_ai_credit_micros: "0",
    trailing_minute_subscription_tokens: "0",
    cache_hit_basis_points: 7500,
    session_cost_cap_micros_usd: null,
    daily_cost_cap_micros_usd: null,
    session_ai_credit_cap_micros: null,
    daily_ai_credit_cap_micros: null,
    session_token_cap: null,
    daily_token_cap: null,
    spend_rate_alarm_micros_usd_per_minute: null,
    ai_credit_rate_alarm_micros_per_minute: null,
    token_rate_alarm_per_minute: null,
    hard_cap_reached: false,
    session_monetary_accounting_complete: true,
    daily_monetary_accounting_complete: true,
    session_subscription_quota_entries: "0",
    session_cost_unavailable_entries: "0",
    session_non_usd_monetary_entries: "0",
    daily_subscription_quota_entries: "0",
    daily_cost_unavailable_entries: "0",
    daily_non_usd_monetary_entries: "0",
  }
}

function frameAnsi(frame: CapturedFrame): string {
  const output: string[] = ["\x1b[2J\x1b[H\x1b[?25l"]
  for (const [row, line] of frame.lines.entries()) {
    output.push(`\x1b[${row + 1};1H`)
    for (const span of line.spans) {
      const [red, green, blue, alpha] = span.fg.toInts()
      const [bgRed, bgGreen, bgBlue, bgAlpha] = span.bg.toInts()
      const attributes = getBaseAttributes(span.attributes)
      const codes = [
        "0",
        alpha > 0 ? `38;2;${red};${green};${blue}` : "39",
        bgAlpha > 0 ? `48;2;${bgRed};${bgGreen};${bgBlue}` : "49",
        ...(attributes & TextAttributes.BOLD ? ["1"] : []),
        ...(attributes & TextAttributes.DIM ? ["2"] : []),
        ...(attributes & TextAttributes.ITALIC ? ["3"] : []),
        ...(attributes & TextAttributes.UNDERLINE ? ["4"] : []),
        ...(attributes & TextAttributes.STRIKETHROUGH ? ["9"] : []),
      ]
      const padding = " ".repeat(Math.max(0, span.width - Bun.stringWidth(span.text)))
      output.push(`\x1b[${codes.join(";")}m${span.text}${padding}`)
    }
  }
  output.push("\x1b[0m\x1b[?25l")
  return output.join("")
}

async function writeRasterPng(frame: CapturedFrame, outputPath: string): Promise<void> {
  const cellWidth = 8
  const lineHeight = 18
  const padding = 2
  const canvas = createCanvas(
    frame.cols * cellWidth + padding * 2,
    frame.rows * lineHeight + padding * 2,
  )
  const context = canvas.getContext("2d")
  context.fillStyle = kennelTheme.background
  context.fillRect(0, 0, canvas.width, canvas.height)
  context.textBaseline = "top"

  for (const [row, line] of frame.lines.entries()) {
    let column = 0
    for (const span of line.spans) {
      const attributes = getBaseAttributes(span.attributes)
      const left = padding + column * cellWidth
      const top = padding + row * lineHeight
      const right = left + span.width * cellWidth
      context.globalAlpha = 1
      context.fillStyle = rgbaCss(span.bg.toInts())
      context.fillRect(left, top, span.width * cellWidth, lineHeight)
      if (span.text !== "") {
        const weight = attributes & TextAttributes.BOLD ? "700 " : ""
        const italic = attributes & TextAttributes.ITALIC ? "italic " : ""
        context.font = `${italic}${weight}13px Menlo, "DejaVu Sans Mono", monospace`
        context.globalAlpha = attributes & TextAttributes.DIM ? 0.62 : 1
        context.fillStyle = rgbaCss(span.fg.toInts())
        context.fillText(span.text, left, top + 1)
        context.lineWidth = 1
        context.strokeStyle = rgbaCss(span.fg.toInts())
        if (attributes & TextAttributes.UNDERLINE) {
          context.beginPath()
          context.moveTo(left, top + lineHeight - 2)
          context.lineTo(right, top + lineHeight - 2)
          context.stroke()
        }
        if (attributes & TextAttributes.STRIKETHROUGH) {
          context.beginPath()
          context.moveTo(left, top + Math.floor(lineHeight / 2))
          context.lineTo(right, top + Math.floor(lineHeight / 2))
          context.stroke()
        }
      }
      column += span.width
    }
  }
  context.globalAlpha = 1
  await writeFile(outputPath, await canvas.encode("png"))
}

function rgbaCss(channels: readonly number[]): string {
  const [red = 0, green = 0, blue = 0, alpha = 255] = channels
  return `rgba(${red}, ${green}, ${blue}, ${alpha / 255})`
}

function rgbHex(red: number, green: number, blue: number): string {
  const channel = (value: number) => value.toString(16).padStart(2, "0")
  return `#${channel(red)}${channel(green)}${channel(blue)}`
}
