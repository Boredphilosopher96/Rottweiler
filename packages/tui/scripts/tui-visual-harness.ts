import { toolOutputBuffer } from "../src/state/display-buffer"
import { createStreamingTail } from "../src/state/model"
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

type VisualScenario = "conversation" | "command-palette" | "approval" | "tools" | "theme-browser" | "settings-browser" | "mcp-browser" | "session-review"

const TOOLS_FIXTURE_NOW_MS = Date.parse("2026-01-01T12:00:41.000Z")
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
    ...(scenarioInput === "tools" ? { nowMs: () => TOOLS_FIXTURE_NOW_MS } : {}),
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
  } else if (scenarioInput === "tools") {
    actions.push("pressed Ctrl+P through the renderer input path")
    setup.mockInput.pressKey("p", { ctrl: true })
    actions.push("typed view tools into the focused production query input")
    await setup.mockInput.typeText("view tools")
    await setup.flush()
    actions.push("pressed Enter to activate the selected View tools action")
    setup.mockInput.pressEnter()
    await setup.flush()
  } else if (scenarioInput === "theme-browser") {
    actions.push("typed /the into the production composer input")
    await setup.mockInput.typeText("/the")
    actions.push("pressed Enter to activate the /theme slash completion")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    await setup.flush()
  } else if (scenarioInput === "settings-browser") {
    actions.push("typed /sett into the production composer input")
    await setup.mockInput.typeText("/sett")
    actions.push("pressed Enter to activate the /settings slash completion")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    await setup.flush()
  } else if (scenarioInput === "mcp-browser") {
    actions.push("typed /mc into the production composer input")
    await setup.mockInput.typeText("/mc")
    actions.push("pressed Enter to activate the /mcp slash completion")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    await setup.flush()
    actions.push("pressed Down twice to select docs.remote through the retained list")
    setup.mockInput.pressArrow("down")
    setup.mockInput.pressArrow("down")
    await setup.flush()
  } else if (scenarioInput === "session-review") {
    actions.push("typed /rev into the production composer input")
    await setup.mockInput.typeText("/rev")
    actions.push("pressed Enter to activate the /review slash completion")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
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
    ...(scenarioInput === "theme-browser" ? themeBrowserLayoutAssertions(app) : []),
    ...(scenarioInput === "settings-browser" ? settingsBrowserLayoutAssertions(app, "wide") : []),
    ...(scenarioInput === "mcp-browser" ? mcpBrowserLayoutAssertions(app, "wide") : []),
    ...(scenarioInput === "session-review" ? sessionReviewLayoutAssertions(app, "wide") : []),
  ]
  await writeEvidence(outputDirectory, scenarioInput, styledFrame, characterFrame, actions, assertions)

  const failed = [...assertions.filter((assertion) => !assertion.passed)]
  if (scenarioInput === "settings-browser") {
    actions.push("resized the production renderer to 72 by 18 columns")
    setup.resize(72, 18)
    await setup.flush()
    const narrowCharacterFrame = setup.captureCharFrame()
    const narrowStyledFrame = setup.captureSpans()
    const narrowAssertions = [
      ...settingsBrowserNarrowAssertions(narrowCharacterFrame, narrowStyledFrame),
      ...settingsBrowserLayoutAssertions(app, "narrow"),
    ]
    await writeEvidence(
      outputDirectory,
      "settings-browser-narrow",
      narrowStyledFrame,
      narrowCharacterFrame,
      actions,
      narrowAssertions,
    )
    failed.push(...narrowAssertions.filter((assertion) => !assertion.passed))
  } else if (scenarioInput === "mcp-browser") {
    actions.push("resized the production renderer to 72 by 18 columns")
    setup.resize(72, 18)
    await setup.flush()
    const narrowCharacterFrame = setup.captureCharFrame()
    const narrowStyledFrame = setup.captureSpans()
    const narrowAssertions = [
      ...mcpBrowserNarrowAssertions(narrowCharacterFrame, narrowStyledFrame),
      ...mcpBrowserLayoutAssertions(app, "narrow"),
    ]
    await writeEvidence(
      outputDirectory,
      "mcp-browser-narrow",
      narrowStyledFrame,
      narrowCharacterFrame,
      actions,
      narrowAssertions,
    )
    failed.push(...narrowAssertions.filter((assertion) => !assertion.passed))
  } else if (scenarioInput === "session-review") {
    actions.push("resized the production renderer to 72 by 18 columns")
    setup.resize(72, 18)
    await setup.flush()
    const narrowCharacterFrame = setup.captureCharFrame()
    const narrowStyledFrame = setup.captureSpans()
    const narrowAssertions = [
      ...sessionReviewNarrowAssertions(narrowCharacterFrame, narrowStyledFrame),
      ...sessionReviewLayoutAssertions(app, "narrow"),
    ]
    await writeEvidence(
      outputDirectory,
      "session-review-narrow",
      narrowStyledFrame,
      narrowCharacterFrame,
      actions,
      narrowAssertions,
    )
    failed.push(...narrowAssertions.filter((assertion) => !assertion.passed))
  }
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
  return value === "conversation" || value === "command-palette" || value === "approval" || value === "tools" || value === "theme-browser" || value === "settings-browser" || value === "mcp-browser" || value === "session-review"
}

function scenarioAssertions(scenario: VisualScenario): readonly string[] {
  switch (scenario) {
    case "conversation":
      return ["you", "● rottweiler", "reasoning", "AGENTS", "TASKS", "CHANGED", "SESSION"]
    case "command-palette":
      return ["COMMAND PALETTE", "context", "Compact context", "Manage context"]
    case "approval":
      return ["Permission required", "Terminal command", "Allow once"]
    case "tools":
      return [
        "● rottweiler  running tools",
        "THIS TURN",
        "tools    6",
        "live     1",
        "denied   1",
        "Esc Esc to interrupt",
        "Next sends when this turn ends",
      ]
    case "theme-browser":
      return [
        "THEME   34 themes   /theme",
        "Filter themes…",
        "opencode  dark · 52 roles resolved · live sample",
        "Markdown roles",
        "Themes change semantic roles, not layout.",
        "↑↓ preview · ⏎ apply · esc cancel",
      ]
    case "settings-browser":
      return [
        "SETTINGS   /settings",
        "Filter settings…",
        "MODEL & ROUTING",
        "Fast thinking",
        "current    medium",
        "choices    low · medium · high",
        "Enter choose · Esc close",
      ]
    case "mcp-browser":
      return [
        "MCP   4 servers · 1 ready · 12 tools   /mcp",
        "Filter MCP connections…",
        "docs.remote · Connected · 6 tools",
        "Approval review",
        "transport   streamable_http",
        "fingerprint sha256:docs",
        "Enter manage · Esc close",
      ]
    case "session-review":
      return [
        "SESSION REVIEW   3 files  +4 −2   2 pending",
        "src/cursor.rs  +2 −1",
        "THIS FILE",
        "lines     +2 −1",
        "DECISIONS",
        "1 accepted",
        "fingerprint",
      ]
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
  if (scenario === "tools") {
    const lines = characterFrame.split("\n")
    return [
      ...assertions,
      positionAssertion(lines, "Tools header begins at column 1", 0, 1, "● rottweiler  running tools"),
      positionAssertion(lines, "turn rail divider is fixed at column 74", 0, 74, "│"),
      positionAssertion(lines, "turn rail content begins at column 75", 0, 75, "THIS TURN"),
      frameWidthAssertion(lines),
      {
        name: "primary divider spans exactly rows 0 through 26",
        passed: lines.slice(0, 27).every((line) => line[74] === "│") &&
          lines[27]?.[74] !== "│" &&
          lines[31]?.[74] !== "│",
        expected: "divider at column 74 only for rows 0-26",
        actual: lines.map((line, row) => line[74] === "│" ? row : -1).filter((row) => row >= 0).join(","),
      },
      {
        name: "complete tool subject is not split or injected with spaces",
        passed: characterFrame.includes("bun test test/components.test.ts") &&
          !characterFrame.includes("bun test test/components....ts") &&
          !characterFrame.includes("r e a s o n") &&
          !characterFrame.includes("e d i t"),
        expected: "intact complete text runs",
        actual: characterFrame.includes("bun test test/components.test.ts") ? "intact" : "missing or clipped",
      },
      {
        name: "unsupported target sections and claims are absent",
        passed: !characterFrame.includes("DIAGNOSTICS") &&
          !characterFrame.includes("BACKGROUND") &&
          !characterFrame.includes("matched rule") &&
          !characterFrame.includes("by you"),
        expected: "no unsupported sections, rule, or actor provenance",
        actual: "checked terminal cells",
      },
      colorAtTextAssertion(styledFrame, lines, "Tools product marker uses primary", "● rottweiler", kennelTheme.primary),
      colorAtTextAssertion(styledFrame, lines, "running outcome uses info", "live · 00:41", kennelTheme.info),
      colorAtTextAssertion(styledFrame, lines, "denied outcome uses error", "denied · 00:05", kennelTheme.error),
      colorAtTextAssertion(styledFrame, lines, "turn rail heading stays readable", "THIS TURN", kennelTheme.text),
    ]
  }
  if (scenario === "theme-browser") {
    const lines = characterFrame.split("\n")
    const malformedSpacing = ["s y s t e m", "r e a s o n", "M a r k d o w n", "a p p l y T h e m e"]
    return [
      ...assertions,
      positionAssertion(lines, "theme heading starts at the design column", 0, 1, "THEME   34 themes   /theme"),
      positionAssertion(lines, "list/detail divider is fixed at column 34", 0, 34, "│"),
      positionAssertion(lines, "detail content starts at column 36", 0, 36, "opencode  dark · 52 roles resolved · live sample"),
      frameWidthAssertion(lines),
      {
        name: "theme names and semantic samples use complete text runs",
        passed: characterFrame.includes("catppuccin") && malformedSpacing.every((text) => !characterFrame.includes(text)),
        expected: "intact names and semantic sample text",
        actual: malformedSpacing.filter((text) => characterFrame.includes(text)).join(",") || "intact",
      },
      {
        name: "detail content stays inside its pane",
        passed: lines.slice(0, 27).every((line) => (line[34] ?? "") === "│"),
        expected: "divider preserved from rows 0 through 26",
        actual: lines.slice(0, 27).map((line, row) => line[34] === "│" ? row : -1).filter((row) => row >= 0).join(","),
      },
      {
        name: "theme surface fully occludes the prior conversation and context rail",
        passed: !characterFrame.includes("AGENTS") &&
          !characterFrame.includes("▌ you") &&
          !characterFrame.includes("● rottweiler") &&
          !characterFrame.includes("\n╎"),
        expected: "no prior screen labels or gutter glyphs",
        actual: ["AGENTS", "▌ you", "● rottweiler", "\n╎"].filter((text) => characterFrame.includes(text)).join(",") || "occluded",
      },
      colorAssertion(styledFrame, "selection marker uses primary", 24, 1, kennelTheme.primary, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "background swatch uses the selected theme", 24, 19, kennelTheme.background, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "primary swatch uses the selected theme", 24, 21, kennelTheme.primary, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "accent swatch uses the selected theme", 24, 23, kennelTheme.accent, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "success swatch uses the selected theme", 24, 25, kennelTheme.success, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "error swatch uses the selected theme", 24, 27, kennelTheme.error, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "detail heading uses selected primary", 0, 36, kennelTheme.primary, kennelTheme.background),
      backgroundAssertion(styledFrame, "theme surface occludes underlying content", 0, 0, kennelTheme.background),
    ]
  }
  if (scenario === "settings-browser") {
    const lines = characterFrame.split("\n")
    const prohibited = ["save", "reset", "discard", "diff", ".rottweiler/config.toml"]
    const malformedSpacing = ["s e t t i n g", "F a s t", "c u r r e n t", "a p p l y"]
    return [
      ...assertions,
      positionAssertion(lines, "settings heading starts at the design column", 0, 1, "SETTINGS   /settings"),
      positionAssertion(lines, "settings divider is fixed at column 30", 0, 30, "│"),
      positionAssertion(lines, "settings detail content starts at column 32", 0, 32, "Fast thinking"),
      frameSizeAssertion(lines, 110, 32),
      {
        name: "settings divider spans exactly the primary surface",
        passed: lines.slice(0, 27).every((line) => line[30] === "│") && lines[27]?.[30] !== "│",
        expected: "divider at column 30 only for rows 0-26",
        actual: lines.map((line, row) => line[30] === "│" ? row : -1).filter((row) => row >= 0).join(","),
      },
      {
        name: "settings labels use complete text runs",
        passed: malformedSpacing.every((text) => !characterFrame.includes(text)),
        expected: "intact settings labels and details",
        actual: malformedSpacing.filter((text) => characterFrame.includes(text)).join(",") || "intact",
      },
      settingsOcclusionAssertion(characterFrame),
      unsupportedSettingsClaimsAssertion(characterFrame, prohibited),
      colorAssertion(styledFrame, "settings selection marker uses primary", 4, 1, kennelTheme.primary, kennelTheme.backgroundPanel),
      colorAssertion(styledFrame, "settings divider uses subtle border", 0, 30, kennelTheme.borderSubtle),
      colorAssertion(styledFrame, "settings detail heading remains readable", 0, 32, kennelTheme.text, kennelTheme.background),
      backgroundAssertion(styledFrame, "settings surface occludes underlying content", 0, 0, kennelTheme.background),
    ]
  }
  if (scenario === "mcp-browser") {
    const lines = characterFrame.split("\n")
    return [
      ...assertions,
      positionAssertion(lines, "MCP heading starts at the design column", 0, 1, "MCP   4 servers · 1 ready · 12 tools   /mcp"),
      positionAssertion(lines, "MCP divider is fixed at column 73", 0, 73, "│"),
      positionAssertion(lines, "MCP detail content starts at column 75", 0, 75, "docs.remote"),
      frameSizeAssertion(lines, 110, 32),
      {
        name: "MCP divider spans exactly the primary surface",
        passed: lines.slice(0, 27).every((line) => line[73] === "│") && lines[27]?.[73] !== "│",
        expected: "divider at column 73 only for rows 0-26",
        actual: lines.map((line, row) => line[73] === "│" ? row : -1).filter((row) => row >= 0).join(","),
      },
      {
        name: "MCP names remain contiguous complete text",
        passed: characterFrame.includes("docs.remote") && characterFrame.includes("broken.remote") &&
          !characterFrame.includes("d o c s") && !characterFrame.includes("b r o k e n"),
        expected: "contiguous server names",
        actual: "checked exact terminal cells",
      },
      mcpOcclusionAssertion(characterFrame),
      unsupportedMcpClaimsAssertion(characterFrame),
      colorAssertion(styledFrame, "connected MCP state uses success", 5, 17, kennelTheme.success),
      colorAssertion(styledFrame, "failed MCP state uses error", 7, 19, kennelTheme.error),
      backgroundAssertion(styledFrame, "MCP surface occludes underlying content", 0, 0, kennelTheme.background),
    ]
  }
  if (scenario === "session-review") {
    const lines = characterFrame.split("\n")
    const malformedSpacing = ["S E S S I O N", "s r c /", "c u r s o r", "f i n g e r p r i n t"]
    const unsupported = ["WORKTREE", "stash", "by       edit", "accept all"].filter(
      (claim) => characterFrame.includes(claim),
    )
    return [
      ...assertions,
      positionAssertion(lines, "review heading starts at column 1", 0, 1, "SESSION REVIEW"),
      positionAssertion(lines, "review divider is fixed at column 73", 0, 73, "│"),
      positionAssertion(lines, "review detail starts at column 75", 0, 75, "THIS FILE"),
      frameSizeAssertion(lines, 110, 32),
      {
        name: "review divider spans exactly the primary surface",
        passed: lines.slice(0, 27).every((line) => line[73] === "│") && lines[27]?.[73] !== "│",
        expected: "divider at column 73 only for rows 0-26",
        actual: lines.map((line, row) => line[73] === "│" ? row : -1).filter((row) => row >= 0).join(","),
      },
      {
        name: "review labels use complete text runs",
        passed: malformedSpacing.every((text) => !characterFrame.includes(text)),
        expected: "intact review paths and labels",
        actual: malformedSpacing.filter((text) => characterFrame.includes(text)).join(",") || "intact",
      },
      {
        name: "review surface has no rounded modal frame",
        passed: !characterFrame.includes("╭─ Session review"),
        expected: "no rounded modal border",
        actual: "checked terminal cells",
      },
      {
        name: "unsupported worktree and bulk-decision claims are absent",
        passed: unsupported.length === 0,
        expected: "no unsupported worktree, actor, or accept-all claims",
        actual: unsupported.join(",") || "absent",
      },
      colorAtTextAssertion(styledFrame, lines, "review heading uses secondary", "SESSION REVIEW", kennelTheme.secondary),
      colorAssertion(styledFrame, "selected pending state uses warning", 1, 85, kennelTheme.warning),
      colorAtTextAssertion(styledFrame, lines, "accepted state uses success", "✓", kennelTheme.success),
      backgroundAssertion(styledFrame, "review surface occludes the conversation", 0, 0, kennelTheme.background),
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

function themeBrowserLayoutAssertions(
  app: ReturnType<typeof createRottweilerApp>,
): readonly VisualAssertion[] {
  const browser = app.themeBrowser
  return [
    exactValueAssertion("theme surface begins at column 0", browser.x, 0),
    exactValueAssertion("theme surface begins at row 0", browser.y, 0),
    exactValueAssertion("theme surface is 110 cells wide", browser.width, 110),
    exactValueAssertion("theme surface is 27 rows tall", browser.height, 27),
    exactValueAssertion("theme list pane begins at column 1", browser.listPane.x, 1),
    exactValueAssertion("theme list pane is 33 cells wide", browser.listPane.width, 33),
    exactValueAssertion("theme divider is one cell wide", browser.divider.width, 1),
    exactValueAssertion("theme divider is fixed at column 34", browser.divider.x, 34),
    exactValueAssertion("theme detail pane begins at column 35", browser.detailPane.x, 35),
    exactValueAssertion("theme detail pane is 74 cells wide", browser.detailPane.width, 74),
    {
      name: "theme browser owns focus after slash activation",
      passed: browser.input.focused,
      expected: "focused theme query",
      actual: browser.input.focused ? "focused theme query" : "not focused",
    },
  ]
}

function settingsBrowserLayoutAssertions(
  app: ReturnType<typeof createRottweilerApp>,
  size: "wide" | "narrow",
): readonly VisualAssertion[] {
  const browser = app.settingsBrowser
  if (size === "narrow") {
    return [
      exactValueAssertion("narrow settings surface begins at column 0", browser.x, 0),
      exactValueAssertion("narrow settings surface begins at row 0", browser.y, 0),
      exactValueAssertion("narrow settings surface is 72 cells wide", browser.width, 72),
      exactValueAssertion("narrow settings surface is 13 rows tall", browser.height, 13),
      exactValueAssertion("narrow settings list begins at column 1", browser.listPane.x, 1),
      exactValueAssertion("narrow settings list is 70 cells wide", browser.listPane.width, 70),
      {
        name: "narrow settings uses the single-pane layout",
        passed: browser.layoutMode === "single" && !browser.divider.visible && !browser.detailPane.visible,
        expected: "single pane with hidden divider and detail pane",
        actual: `${browser.layoutMode}; divider ${browser.divider.visible}; detail ${browser.detailPane.visible}`,
      },
      {
        name: "narrow settings preserves compact current-value detail",
        passed: browser.compactDetail.visible && browser.compactDetail.plainText.includes("current    medium"),
        expected: "visible compact current    medium",
        actual: browser.compactDetail.visible ? browser.compactDetail.plainText : "hidden",
      },
    ]
  }
  return [
    exactValueAssertion("settings surface begins at column 0", browser.x, 0),
    exactValueAssertion("settings surface begins at row 0", browser.y, 0),
    exactValueAssertion("settings surface is 110 cells wide", browser.width, 110),
    exactValueAssertion("settings surface is 27 rows tall", browser.height, 27),
    exactValueAssertion("settings list pane begins at column 1", browser.listPane.x, 1),
    exactValueAssertion("settings list pane is 29 cells wide", browser.listPane.width, 29),
    exactValueAssertion("settings divider is fixed at column 30", browser.divider.x, 30),
    exactValueAssertion("settings detail begins at column 31", browser.detailPane.x, 31),
    exactValueAssertion("settings detail is 78 cells wide", browser.detailPane.width, 78),
    {
      name: "settings browser owns focus after slash activation",
      passed: browser.input.focused,
      expected: "focused settings query",
      actual: browser.input.focused ? "focused settings query" : "not focused",
    },
  ]
}

function settingsBrowserNarrowAssertions(
  characterFrame: string,
  styledFrame: CapturedFrame,
): readonly VisualAssertion[] {
  const lines = characterFrame.split("\n")
  const required = [
    "SETTINGS   /settings",
    "Filter settings…",
    "MODEL & ROUTING",
    "Fast thinking",
    "current    medium",
  ]
  return [
    ...required.map((text): VisualAssertion => ({
      name: `narrow visible text: ${text}`,
      passed: characterFrame.includes(text),
      expected: text,
      actual: characterFrame.includes(text) ? text : "missing",
    })),
    frameSizeAssertion(lines, 72, 18),
    {
      name: "narrow settings does not retain a split-pane divider",
      passed: lines.slice(0, 13).every((line) => line[30] !== "│"),
      expected: "no settings divider at column 30",
      actual: lines.map((line, row) => line[30] === "│" ? row : -1).filter((row) => row >= 0).join(",") || "hidden",
    },
    settingsOcclusionAssertion(characterFrame),
    unsupportedSettingsClaimsAssertion(characterFrame, ["save", "reset", "discard", "diff", ".rottweiler/config.toml"]),
    colorAssertion(styledFrame, "narrow settings selection marker uses primary", 4, 1, kennelTheme.primary, kennelTheme.backgroundPanel),
    backgroundAssertion(styledFrame, "narrow settings surface occludes underlying content", 0, 0, kennelTheme.background),
  ]
}

function mcpBrowserLayoutAssertions(
  app: ReturnType<typeof createRottweilerApp>,
  size: "wide" | "narrow",
): readonly VisualAssertion[] {
  const browser = app.mcpBrowser
  if (size === "narrow") {
    return [
      exactValueAssertion("narrow MCP surface begins at column 0", browser.x, 0),
      exactValueAssertion("narrow MCP surface begins at row 0", browser.y, 0),
      exactValueAssertion("narrow MCP surface is 72 cells wide", browser.width, 72),
      exactValueAssertion("narrow MCP surface is 13 rows tall", browser.height, 13),
      exactValueAssertion("narrow MCP list is 70 cells wide", browser.listPane.width, 70),
      {
        name: "narrow MCP uses the single-pane layout",
        passed: browser.layoutMode === "single" && !browser.divider.visible && !browser.detailPane.visible,
        expected: "single pane with hidden divider and detail pane",
        actual: `${browser.layoutMode}; divider ${browser.divider.visible}; detail ${browser.detailPane.visible}`,
      },
      {
        name: "narrow MCP preserves compact selected detail",
        passed: browser.compactDetail.visible && browser.compactDetail.plainText.includes("Connected · enabled · approved"),
        expected: "visible compact selected server truth",
        actual: browser.compactDetail.visible ? browser.compactDetail.plainText : "hidden",
      },
    ]
  }
  return [
    exactValueAssertion("MCP surface begins at column 0", browser.x, 0),
    exactValueAssertion("MCP surface begins at row 0", browser.y, 0),
    exactValueAssertion("MCP surface is 110 cells wide", browser.width, 110),
    exactValueAssertion("MCP surface is 27 rows tall", browser.height, 27),
    exactValueAssertion("MCP list pane begins at column 1", browser.listPane.x, 1),
    exactValueAssertion("MCP list pane is 72 cells wide", browser.listPane.width, 72),
    exactValueAssertion("MCP divider is fixed at column 73", browser.divider.x, 73),
    exactValueAssertion("MCP detail begins at column 74", browser.detailPane.x, 74),
    exactValueAssertion("MCP detail pane is 35 cells wide", browser.detailPane.width, 35),
    {
      name: "MCP browser owns focus after slash activation",
      passed: browser.input.focused,
      expected: "focused MCP query",
      actual: browser.input.focused ? "focused MCP query" : "not focused",
    },
  ]
}

function mcpBrowserNarrowAssertions(
  characterFrame: string,
  styledFrame: CapturedFrame,
): readonly VisualAssertion[] {
  const lines = characterFrame.split("\n")
  const required = [
    "MCP   4 servers · 1 ready · 12 tools   /mcp",
    "Filter MCP connections…",
    "docs.remote · Connected · 6 tools",
    "Connected · enabled · approved",
  ]
  return [
    ...required.map((text): VisualAssertion => ({
      name: `narrow visible text: ${text}`,
      passed: characterFrame.includes(text),
      expected: text,
      actual: characterFrame.includes(text) ? text : "missing",
    })),
    frameSizeAssertion(lines, 72, 18),
    {
      name: "narrow MCP does not retain its split divider",
      passed: lines.slice(0, 13).every((line) => line[73] !== "│"),
      expected: "no MCP divider",
      actual: "checked exact terminal cells",
    },
    mcpOcclusionAssertion(characterFrame),
    unsupportedMcpClaimsAssertion(characterFrame),
    colorAtTextAssertion(styledFrame, lines, "narrow connected MCP state uses success", "Connected", kennelTheme.success),
    backgroundAssertion(styledFrame, "narrow MCP surface occludes underlying content", 0, 0, kennelTheme.background),
  ]
}

function sessionReviewLayoutAssertions(
  app: ReturnType<typeof createRottweilerApp>,
  size: "wide" | "narrow",
): readonly VisualAssertion[] {
  const review = app.reviewPanel
  if (size === "narrow") {
    return [
      exactValueAssertion("narrow review surface begins at column 0", review.x, 0),
      exactValueAssertion("narrow review surface begins at row 0", review.y, 0),
      exactValueAssertion("narrow review surface is 72 cells wide", review.width, 72),
      exactValueAssertion("narrow review surface is 13 rows tall", review.height, 13),
      exactValueAssertion("narrow review content is 72 cells wide", review.leftPane.width, 72),
      {
        name: "narrow review removes the detail rail",
        passed: !review.rightRail.visible,
        expected: "hidden detail rail",
        actual: review.rightRail.visible ? "visible" : "hidden",
      },
      {
        name: "narrow review retains the exact selected diff",
        passed: review.diff.visible && review.diff.diff.includes("+new") && review.diff.diff.includes("+added"),
        expected: "visible selected diff",
        actual: review.diff.visible ? review.diff.diff : "hidden",
      },
    ]
  }
  return [
    exactValueAssertion("review surface begins at column 0", review.x, 0),
    exactValueAssertion("review surface begins at row 0", review.y, 0),
    exactValueAssertion("review surface is 110 cells wide", review.width, 110),
    exactValueAssertion("review surface is 27 rows tall", review.height, 27),
    exactValueAssertion("review content region is 73 cells wide", review.leftPane.width, 73),
    exactValueAssertion("review detail divider is fixed at column 73", review.rightRail.x, 73),
    exactValueAssertion("review detail region is 37 cells wide", review.rightRail.width, 37),
    exactValueAssertion("review detail content begins at column 75", review.details.x, 75),
    {
      name: "review file list owns focus after slash activation",
      passed: review.files.focused,
      expected: "focused review file list",
      actual: review.files.focused ? "focused review file list" : "not focused",
    },
  ]
}

function sessionReviewNarrowAssertions(
  characterFrame: string,
  styledFrame: CapturedFrame,
): readonly VisualAssertion[] {
  const lines = characterFrame.split("\n")
  const required = [
    "SESSION REVIEW   3 files  +4 −2   2 pending",
    "src/cursor.rs  +2 −1",
    "accept",
    "revert",
  ]
  return [
    ...required.map((text): VisualAssertion => ({
      name: `narrow visible text: ${text}`,
      passed: characterFrame.includes(text),
      expected: text,
      actual: characterFrame.includes(text) ? text : "missing",
    })),
    frameSizeAssertion(lines, 72, 18),
    {
      name: "narrow review has no split divider",
      passed: lines.slice(0, 13).every((line) => line[73] !== "│"),
      expected: "no review divider",
      actual: "checked exact terminal cells",
    },
    colorAtTextAssertion(styledFrame, lines, "narrow review heading uses secondary", "SESSION REVIEW", kennelTheme.secondary),
    backgroundAssertion(styledFrame, "narrow review surface occludes the conversation", 0, 0, kennelTheme.background),
  ]
}

function mcpOcclusionAssertion(characterFrame: string): VisualAssertion {
  const leaked = ["AGENTS", "▌ you", "● rottweiler", "\n╎"].filter((text) => characterFrame.includes(text))
  return {
    name: "MCP surface fully occludes prior conversation and context",
    passed: leaked.length === 0,
    expected: "no prior screen labels or gutter glyphs",
    actual: leaked.join(",") || "occluded",
  }
}

function unsupportedMcpClaimsAssertion(characterFrame: string): VisualAssertion {
  const unsupported = [
    "context tokens",
    "eager cost",
    "capability",
    "allowlist",
    "reauthorize",
    "sandbox",
    "TOON",
    "rw serve --mcp",
    "Retry server",
  ].filter((claim) => characterFrame.toLocaleLowerCase().includes(claim.toLocaleLowerCase()))
  return {
    name: "unsupported MCP capability and server-operation claims are absent",
    passed: unsupported.length === 0,
    expected: "no unsupported MCP claims",
    actual: unsupported.join(",") || "absent",
  }
}

function settingsOcclusionAssertion(characterFrame: string): VisualAssertion {
  const leaked = ["AGENTS", "▌ you", "● rottweiler", "\n╎"].filter((text) => characterFrame.includes(text))
  return {
    name: "settings surface fully occludes prior conversation and context",
    passed: leaked.length === 0,
    expected: "no prior screen labels or gutter glyphs",
    actual: leaked.join(",") || "occluded",
  }
}

function unsupportedSettingsClaimsAssertion(
  characterFrame: string,
  prohibited: readonly string[],
): VisualAssertion {
  const found = prohibited.filter((claim) => claim.startsWith(".")
    ? characterFrame.includes(claim)
    : new RegExp(`\\b${claim}\\b`, "i").test(characterFrame))
  return {
    name: "unsupported staged-editor and config-file claims are absent",
    passed: found.length === 0,
    expected: "no save, reset, discard, diff, or config path claims",
    actual: found.join(",") || "absent",
  }
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
  return frameSizeAssertion(lines, 110, 32)
}

function frameSizeAssertion(
  lines: readonly string[],
  columns: number,
  rows: number,
): VisualAssertion {
  const widths = lines.slice(0, rows).map((line) => Bun.stringWidth(line))
  const actual = `${widths.length} rows; widths ${Math.min(...widths)}-${Math.max(...widths)}`
  return {
    name: `every terminal row retains the complete ${columns}-cell frame`,
    passed: widths.length === rows && widths.every((width) => width === columns),
    expected: `${rows} rows; widths ${columns}-${columns}`,
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

function backgroundAssertion(
  frame: CapturedFrame,
  name: string,
  row: number,
  column: number,
  expectedBackground: string,
): VisualAssertion {
  const cell = capturedCell(frame, row, column)
  const actual = cell === null ? "missing" : rgbaHex(cell.bg.toInts())
  const expected = normalizeHex(expectedBackground)
  return { name, passed: actual === expected, expected, actual }
}

function colorAtTextAssertion(
  frame: CapturedFrame,
  lines: readonly string[],
  name: string,
  text: string,
  expectedForeground: string,
): VisualAssertion {
  const row = lines.findIndex((line) => line.includes(text))
  const column = row < 0 ? -1 : lines[row]?.indexOf(text) ?? -1
  if (row < 0 || column < 0) {
    return {
      name,
      passed: false,
      expected: `${text} in ${normalizeHex(expectedForeground)}`,
      actual: "text missing",
    }
  }
  return colorAssertion(frame, name, row, column, expectedForeground)
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

async function writeEvidence(
  directory: string,
  artifactName: string,
  styledFrame: CapturedFrame,
  characterFrame: string,
  actions: readonly string[],
  assertions: readonly VisualAssertion[],
): Promise<void> {
  await writeRasterPng(styledFrame, join(directory, `${artifactName}.png`))
  await Promise.all([
    writeFile(join(directory, `${artifactName}.txt`), characterFrame),
    writeFile(join(directory, `${artifactName}.ansi`), frameAnsi(styledFrame)),
    writeFile(join(directory, `${artifactName}.json`), JSON.stringify({
      scenario: artifactName,
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
  if (scenario === "tools") return toolsState()
  if (scenario === "settings-browser") return settingsState()
  if (scenario === "mcp-browser") return mcpState()
  if (scenario === "session-review") return sessionReviewState()
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
    chunks: toolOutputBuffer([]),
    isError: null,
  })
  return {
    ...state,
    streamingTail: createStreamingTail({
      turnId: "2",
      text: "I need permission before running the focused regression suite.",
      thinking: "The command executes workspace code, so it must cross the approval boundary.",
      citations: [],
      toolCallIds: [approval.toolCallId],
      finished: null,
    }),
    tools: { [approval.toolCallId]: approval },
  }
}

function sessionReviewState(): RottweilerState {
  return {
    ...conversationState(),
    review: {
      sessionId: "visual-session",
      files: [
        {
          path: "src/cursor.rs",
          unifiedDiff: "--- a/src/cursor.rs\n+++ b/src/cursor.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+added\n context\n",
          status: "pending",
          truncated: false,
          unrestorableReason: null,
          originalHash: "cursor-before",
          currentHash: "cursor-after",
        },
        {
          path: "packages/tui/src/app.ts",
          unifiedDiff: "--- a/packages/tui/src/app.ts\n+++ b/packages/tui/src/app.ts\n@@ -1 +1 @@\n-before\n+after\n",
          status: "accepted",
          truncated: false,
          unrestorableReason: null,
          originalHash: "app-before",
          currentHash: "app-after",
        },
        {
          path: "generated/report.txt",
          unifiedDiff: "--- /dev/null\n+++ b/generated/report.txt\n@@ -0,0 +1 @@\n+generated\n",
          status: "pending",
          truncated: false,
          unrestorableReason: "original bytes were not checkpointed",
          originalHash: "absent",
          currentHash: "report-after",
        },
      ],
    },
  }
}

function mcpState(): RottweilerState {
  const base = conversationState()
  return {
    ...base,
    commands: [
      ...base.commands,
      { name: "mcp", description: "Inspect and manage MCP connections", usage: "/mcp" },
    ],
    mcpServers: [
      { name: "docs.remote", enabled: true, approved: true, state: { type: "ready" }, tool_count: 6, resource_count: 2, prompt_count: 1 },
      { name: "build.local", enabled: true, approved: true, state: { type: "connecting" }, tool_count: 3, resource_count: 0, prompt_count: 0 },
      { name: "broken.remote", enabled: true, approved: true, state: { type: "failed", message: "TLS certificate rejected" }, tool_count: 2, resource_count: 0, prompt_count: 0 },
      { name: "approval.pending", enabled: true, approved: false, state: { type: "approval_required" }, tool_count: 1, resource_count: 1, prompt_count: 0 },
    ],
    mcpApprovalReview: {
      server: "docs.remote",
      transport: "streamable_http",
      endpoint: "https://docs.example/mcp",
      origin: "user configuration",
      defer_tools: true,
      fingerprint: "sha256:docs",
      previously_approved: true,
    },
  }
}

function settingsState(): RottweilerState {
  return {
    ...conversationState(),
    settings: [
      {
        key: "models.thinking.fast",
        label: "Fast thinking",
        value: "medium",
        choices: ["low", "medium", "high"],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "project.models.default",
        label: "Project default model",
        value: "gpt-5",
        choices: ["gpt-5"],
        provenance: "private project preference",
        appliesImmediately: false,
      },
      {
        key: "permissions.default",
        label: "Default approval policy",
        value: "ask",
        choices: ["ask", "allow", "deny"],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "compaction.auto",
        label: "Automatic compaction",
        value: "true",
        choices: ["true", "false"],
        provenance: "built-in",
        appliesImmediately: false,
      },
      {
        key: "budget.session_token_cap",
        label: "Session token cap",
        value: "250000",
        choices: [],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "budget.warn_at_percent",
        label: "Budget warning",
        value: "80",
        choices: [],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "mcp.servers.docs.enabled",
        label: "MCP · docs",
        value: "true",
        choices: ["true", "false"],
        provenance: "user MCP configuration",
        appliesImmediately: false,
      },
      {
        key: "ui.theme",
        label: "Theme",
        value: "kennel",
        choices: [],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "ui.keybindings.preset",
        label: "Keybinding preset",
        value: "standard",
        choices: ["standard", "vim"],
        provenance: "user",
        appliesImmediately: false,
      },
      {
        key: "telemetry.detail",
        label: "Telemetry detail",
        value: "minimal",
        choices: ["off", "minimal"],
        provenance: "built-in",
        appliesImmediately: false,
      },
    ],
  }
}

function toolsState(): RottweilerState {
  const startedAtMs = TOOLS_FIXTURE_NOW_MS - 41_000
  const makeTool = (
    toolCallId: string,
    callIndex: number,
    extra: Partial<ToolProjection>,
  ): ToolProjection => ({
    toolCallId,
    turnId: "tools-turn",
    name: "read",
    args: { path: `${toolCallId}.ts` },
    status: "finished",
    capabilities: [],
    rationale: null,
    diff: null,
    chunks: toolOutputBuffer([]),
    output: { type: "text", text: "Completed retained output" },
    isError: false,
    callIndex,
    timing: { kind: "closed", startedAtMs, finishedAtMs: startedAtMs + 5_000 },
    ...extra,
  })
  const tools = [
    makeTool("read-app", 0, {
      name: "read",
      args: { path: "packages/tui/src/app.ts" },
      output: { type: "text", text: "Read 5,894 lines" },
    }),
    makeTool("search-workspace", 1, {
      name: "grep",
      args: { pattern: "ToolsWorkspaceRenderable" },
      output: { type: "text", text: "packages/tui/src/app.ts: ToolsWorkspaceRenderable" },
    }),
    makeTool("component-tests", 2, {
      name: "bash",
      args: { command: "bun test test/components.test.ts" },
      status: "running",
      chunks: toolOutputBuffer([{
        stream: "stdout",
        chunk: Array.from({ length: 12 }, (_, index) => `component check ${index + 1} passed`).join("\n"),
      }]),
      output: null,
      isError: null,
      timing: { kind: "open", startedAtMs, lastObservedAtMs: startedAtMs + 40_000 },
    }),
    makeTool("denied-generated-edit", 3, {
      name: "edit",
      args: { path: "generated/output.ts" },
      output: { type: "text", text: "permission denied for tool edit" },
      isError: true,
    }),
    makeTool("write-component", 4, {
      name: "write",
      args: { path: "packages/tui/src/components/tools-workspace.ts" },
      output: { type: "text", text: "Wrote the retained workspace component" },
    }),
    makeTool("explicit-diagnostics", 5, {
      name: "diagnostics",
      args: { path: "packages/tui/src/app.ts" },
      output: { type: "text", text: "No diagnostics." },
    }),
  ]
  return {
    ...createInitialState(),
    connection: { phase: "connected", attempt: 0, error: null, gap: null },
    mode: "execute",
    provider: "openai",
    model: "gpt-5",
    streamingTail: createStreamingTail({
      turnId: "tools-turn",
      text: "",
      thinking: "",
      citations: [],
      toolCallIds: tools.map((tool) => tool.toolCallId),
      finished: null,
    }),
    turns: {
      "tools-turn": {
        turnId: "tools-turn",
        status: "running",
        usage: null,
        cost: null,
        timing: { kind: "open", startedAtMs, lastObservedAtMs: startedAtMs + 40_000 },
      },
    },
    tools: Object.fromEntries(tools.map((item) => [item.toolCallId, item])),
    queuedMessages: [
      { position: "1", content: "Run the complete suite" },
      { position: "2", content: "Inspect the direct raster" },
    ],
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
    chunks: toolOutputBuffer([]),
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
    chunks: toolOutputBuffer([]),
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
    chunks: toolOutputBuffer([]),
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
    streamingTail: createStreamingTail({
      turnId: "2",
      text: "## What changed\n\nThe stream resumes from the last **durable** sequence, not the last delivered frame.\n\n1. `cursor.rs` tracks `durable_seq` independently\n2. `sse.ts` replays from that sequence on reattach\n3. `app.ts` drops the transport-ack fast path",
      thinking: "Two acknowledgements exist here: the transport ack and\nthe durable sequence ack. The client advances its cursor\non the transport ack, so a reconnect replays from a\nsequence the UI already consumed. Keep them separate.",
      citations: [{ uri: "protocol/session-log.md", title: "Reconnect contract" }],
      toolCallIds: [edit.toolCallId, tests.toolCallId, read.toolCallId],
      finished: null,
    }),
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
    timing: { kind: "unknown" },
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
