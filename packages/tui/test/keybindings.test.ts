import { afterEach, describe, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import type { ClientCommand } from "../src/protocol"
import {
  KeybindingConfigurationError,
  compileKeybindings,
  parseKeybindingToml,
  type KeybindingConfiguration,
} from "../src/keybindings"
import { createInitialState } from "../src/state"

describe("configurable TUI keybindings", () => {
  test("keeps the standard map backward-compatible and supports explicit unbinding", () => {
    const standard = compileKeybindings()
    expect(standard.preset).toBe("standard")
    expect(standard.bindings("global").get("ctrl+p")).toBe("open_command_picker")
    expect(standard.bindings("global").get("shift+tab")).toBe("cycle_agent_mode")
    expect(standard.bindings("global").get("ctrl+v")).toBe("paste_image")
    expect(standard.bindings("standard").get("ctrl+e")).toBe("open_external_editor")

    const rebound = compileKeybindings({
      bindings: { global: { open_command_picker: ["ctrl+k"], open_review: [] } },
    })
    expect(rebound.bindings("global").has("ctrl+p")).toBeFalse()
    expect(rebound.bindings("global").has("ctrl+r")).toBeFalse()
    expect(rebound.bindings("global").get("ctrl+k")).toBe("open_command_picker")
  })

  test("rejects conflicts, malformed keys, unknown actions, and oversized TOML", () => {
    expect(() =>
      compileKeybindings({
        bindings: {
          global: {
            open_review: "ctrl+x",
            open_command_picker: "ctrl+x",
          },
        },
      }),
    ).toThrow(KeybindingConfigurationError)
    expect(() =>
      compileKeybindings({
        bindings: { vim_normal: { move_up: "ctrl+ctrl+k" } },
      }),
    ).toThrow("repeats modifier")
    expect(() =>
      compileKeybindings({
        bindings: { vim_normal: { launch_missiles: "m" } },
      } as unknown as KeybindingConfiguration),
    ).toThrow("unknown action")
    expect(() => parseKeybindingToml("x".repeat(64 * 1024 + 1))).toThrow("exceeds 64 KiB")
    expect(() =>
      compileKeybindings({
        bindings: { global: { open_command_picker: "ctrl+c" } },
      }),
    ).toThrow("renderer owns it for immediate exit")
    expect(() =>
      compileKeybindings({
        bindings: { global: { open_command_picker: "return" } },
      }),
    ).toThrow("focused safety panel owns it")
    expect(() =>
      compileKeybindings({
        bindings: { review: { close_overlay: "a" } },
      }),
    ).toThrow("focused safety panel owns it")
  })

  test("parses the documented TOML action map deterministically", () => {
    const configuration = parseKeybindingToml(`
preset = "vim"

[bindings.vim_normal]
open_command_picker = ["space"]
focus_next = []
`)
    const compiled = compileKeybindings(configuration)
    expect(compiled.preset).toBe("vim")
    expect(compiled.bindings("vim_normal").get("space")).toBe("open_command_picker")
    expect(compiled.bindings("vim_normal").has("tab")).toBeFalse()
  })
})

describe("Vim TUI interaction", () => {
  let renderer: TestRenderer | undefined

  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("protects the composer in normal mode and provides basic Vim editing", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { keybindings: { preset: "vim" } })
    renderer.root.add(app)
    await setup.renderOnce()

    await setup.mockInput.typeText("zzzz")
    expect(app.composer.value).toBe("")
    expect(app.statusLine.plainText).toContain("NORMAL · composer")

    setup.mockInput.pressKey("i")
    await setup.mockInput.typeText("hound")
    expect(app.composer.value).toBe("hound")
    expect(app.statusLine.plainText).toContain("INSERT")

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    setup.mockInput.pressKey("h")
    setup.mockInput.pressKey("x")
    expect(app.composer.value).toBe("houn")
    expect(app.statusLine.plainText).toContain("NORMAL · composer")
  })

  test("uses two-stage Escape and normal navigation inside fuzzy pickers", async () => {
    const setup = await createTestRenderer({ width: 92, height: 22, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      keybindings: { preset: "vim" },
      initialState: {
        ...createInitialState(),
        commands: [
          { name: "alpha", description: "first", usage: "/alpha" },
          { name: "beta", description: "second", usage: "/beta" },
        ],
      },
    })
    renderer.root.add(app)
    setup.mockInput.pressKey("p", { ctrl: true })
    await setup.mockInput.typeText("a")
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.input.value).toBe("a")

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.picker.visible).toBeTrue()
    expect(app.statusLine.plainText).toContain("NORMAL · picker")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.picker.visible).toBeFalse()
    expect(app.statusLine.plainText).toContain("NORMAL · composer")
  })

  test("switches normal focus to retained transcript navigation without editing", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const transcript = Array.from({ length: 40 }, (_, index) => ({
      sequenceId: String(index + 1),
      agentTurn: String(index + 1),
      turn: {
        role: "assistant" as const,
        blocks: [{ type: "text" as const, text: `Retained line ${index}` }],
        meta: { synthetic: false, summary: false },
      },
    }))
    const app = createRottweilerApp(renderer, {
      keybindings: { preset: "vim" },
      initialState: { ...createInitialState(), transcript },
    })
    renderer.root.add(app)
    await setup.renderOnce()
    app.transcript.setScrollOffset(0)
    setup.mockInput.pressTab()
    setup.mockInput.pressKey("j")
    await setup.renderOnce()

    expect(app.composer.value).toBe("")
    expect(app.transcript.scroller.scrollTop).toBeGreaterThan(0)
    expect(app.statusLine.plainText).toContain("NORMAL · transcript")
  })

  test("leaves plan, tool, question, and review decisions with their focused safety panels", async () => {
    const setup = await createTestRenderer({ width: 100, height: 28, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const base = createInitialState()
    const app = createRottweilerApp(renderer, {
      keybindings: { preset: "vim" },
      initialState: {
        ...base,
        pendingPlan: {
          title: "Safety plan",
          summary_md: "Approve explicitly.",
          steps: [{ description: "Verify", files_touched: [], verification: "bun test" }],
          open_questions: [],
        },
      },
      sessionId: "session-vim-safety",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    expect(app.statusLine.plainText).toContain("NORMAL · interaction")
    setup.mockInput.pressEnter()
    expect(commands.at(-1)).toMatchObject({ type: "approve_plan", decision: "approve" })

    app.setState({
      ...base,
      tools: {
        safety: {
          toolCallId: "safety",
          turnId: "1",
          name: "bash",
          args: { command: "bun test" },
          status: "awaiting_approval",
          capabilities: ["execute"],
          rationale: "Run tests",
          diff: null,
          chunks: [],
          output: null,
          isError: null,
          callIndex: 0,
        },
      },
    })
    setup.mockInput.pressEnter()
    expect(commands.at(-1)).toMatchObject({
      type: "approve_tool",
      tool_call_id: "safety",
      decision: "allow_once",
    })

    app.setState({
      ...base,
      questions: {
        scope: {
          questionId: "scope",
          turnId: "1",
          answered: false,
          answers: null,
          questions: [{
            id: "scope",
            prompt: "Which scope?",
            response_kind: "select_one",
            options: [
              { value: "focused", label: "Focused", description: "Fast" },
              { value: "full", label: "Full", description: "Complete" },
            ],
          }],
        },
      },
    })
    setup.mockInput.pressEnter()
    expect(commands.at(-1)).toMatchObject({ type: "answer_question", question_id: "scope" })

    const review = {
      sessionId: "session-vim-safety",
      files: [{
        path: "src/safety.rs",
        unifiedDiff: "--- a/src/safety.rs\n+++ b/src/safety.rs\n@@ -1 +1 @@\n-old\n+new\n",
        status: "pending" as const,
        truncated: false,
        unrestorableReason: null,
        originalHash: "old",
        currentHash: "new",
      }],
    }
    app.setState({ ...base, review })
    app.openReview()
    expect(app.statusLine.plainText).toContain("NORMAL · review")
    setup.mockInput.pressKey("a", { shift: true })
    await Bun.sleep(1)
    setup.mockInput.pressKey("r", { shift: true })
    await Bun.sleep(1)
    expect(commands.slice(-2)).toEqual([
      expect.objectContaining({ type: "review_file", decision: "accept" }),
      expect.objectContaining({ type: "review_file", decision: "revert" }),
    ])

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    app.setState(base)
    expect(app.statusLine.plainText).toContain("NORMAL · composer")
    expect(app.composer.editor.focused).toBeTrue()
  })
})
