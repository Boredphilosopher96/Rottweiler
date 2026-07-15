import { afterEach, describe, expect, test } from "bun:test"
import { parseKeypress } from "@opentui/core"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import type { ClientCommand } from "../src/protocol"
import {
  KeybindingConfigurationError,
  compileKeybindings,
  enhancedKeyboardOptions,
  legacyMacNavigationAction,
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

describe("standard TUI keyboard safety", () => {
  let renderer: TestRenderer | undefined

  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("decodes enhanced macOS Command arrows separately from a physical Ctrl+E", () => {
    const commandRight = parseKeypress("\u001b[1;9C", { useKittyKeyboard: true })
    const commandLeft = parseKeypress("\u001b[1;9D", { useKittyKeyboard: true })
    const controlE = parseKeypress("\u001b[101;5u", { useKittyKeyboard: true })

    expect(enhancedKeyboardOptions).toEqual({ allKeysAsEscapes: true })
    expect(commandRight).toMatchObject({ name: "right", super: true, ctrl: false })
    expect(commandLeft).toMatchObject({ name: "left", super: true, ctrl: false })
    expect(controlE).toMatchObject({ name: "e", ctrl: true, source: "kitty" })
    expect(controlE?.super).not.toBe(true)
  })

  test("treats ambiguous legacy macOS Ctrl+A/E bytes as Command-arrow navigation", () => {
    const legacyCommandLeft = parseKeypress("\u0001", { useKittyKeyboard: true })!
    const legacyCommandRight = parseKeypress("\u0005", { useKittyKeyboard: true })!
    const enhancedControlE = parseKeypress("\u001b[101;5u", { useKittyKeyboard: true })!

    expect(legacyMacNavigationAction(legacyCommandLeft, "darwin")).toBe("line_start")
    expect(legacyMacNavigationAction(legacyCommandRight, "darwin")).toBe("line_end")
    expect(legacyMacNavigationAction(enhancedControlE, "darwin")).toBeNull()
    expect(legacyMacNavigationAction(legacyCommandRight, "linux")).toBeNull()
  })

  test("uses double Escape to interrupt an active response", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        turns: { active: { turnId: "active", status: "running", usage: null, cost: null } },
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(commands).toHaveLength(0)
    expect(app.banner.plainText).toContain("Esc again")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(commands.at(-1)).toMatchObject({ type: "interrupt" })
  })

  test("surfaces a rejected double-Escape interrupt", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        turns: { active: { turnId: "active", status: "running", usage: null, cost: null } },
      },
      onCommand(command) {
        if (command.type !== "interrupt") return { type: "accepted" }
        return {
          type: "rejected",
          error: {
            category: "protocol",
            code: "interrupt_rejected",
            message: "The response could not be stopped.",
            retryable: true,
          },
        }
      },
    })
    renderer.root.add(app)

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.state.errors.at(-1)?.code).toBe("interrupt_rejected")
  })

  test("keeps Cmd+Left/Right in the composer and reserves Ctrl+E for the editor", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    let editorCalls = 0
    const app = createRottweilerApp(renderer, {
      platform: "darwin",
      editor: {
        async compose(draft) {
          editorCalls += 1
          return draft
        },
      },
    })
    renderer.root.add(app)
    app.composer.value = "rottweiler"
    app.composer.editor.cursorOffset = 0

    const commandRight = parseKeypress("\u001b[1;9C", { useKittyKeyboard: true })!
    const commandLeft = parseKeypress("\u001b[1;9D", { useKittyKeyboard: true })!
    const controlE = parseKeypress("\u001b[101;5u", { useKittyKeyboard: true })!
    setup.renderer.keyInput.processParsedKey(commandRight)
    expect(app.composer.editor.cursorOffset).toBe(new TextEncoder().encode("rottweiler").length)
    expect(editorCalls).toBe(0)
    setup.renderer.keyInput.processParsedKey(commandLeft)
    expect(app.composer.editor.cursorOffset).toBe(0)
    setup.mockInput.pressArrow("right", { meta: true })
    expect(app.composer.editor.cursorOffset).toBe(new TextEncoder().encode("rottweiler").length)
    setup.mockInput.pressArrow("left", { meta: true })
    expect(app.composer.editor.cursorOffset).toBe(0)
    setup.renderer.keyInput.processParsedKey(controlE)
    await Bun.sleep(0)
    expect(editorCalls).toBe(1)
  })

  test("never opens the editor for the legacy macOS Command+Right byte", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    let editorCalls = 0
    const app = createRottweilerApp(renderer, {
      platform: "darwin",
      editor: {
        async compose(draft) {
          editorCalls += 1
          return draft
        },
      },
    })
    renderer.root.add(app)
    app.composer.value = "rottweiler"
    app.composer.editor.cursorOffset = 0

    const legacyCommandRight = parseKeypress("\u0005", { useKittyKeyboard: true })!
    setup.renderer.keyInput.processParsedKey(legacyCommandRight)
    await Bun.sleep(0)

    expect(app.composer.editor.cursorOffset).toBe(new TextEncoder().encode("rottweiler").length)
    expect(editorCalls).toBe(0)
  })

  test("cycles accepted prompt history without stealing multiline cursor movement", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      onCommand() {
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.composer.value = "first prompt"
    expect(await app.composer.submit()).toBeTrue()
    app.composer.value = "second prompt"
    expect(await app.composer.submit()).toBeTrue()
    app.composer.value = "draft in progress"
    app.composer.focus()

    setup.mockInput.pressArrow("up")
    expect(app.composer.value).toBe("second prompt")
    setup.mockInput.pressArrow("up")
    expect(app.composer.value).toBe("first prompt")
    setup.mockInput.pressArrow("down")
    expect(app.composer.value).toBe("second prompt")
    setup.mockInput.pressArrow("down")
    expect(app.composer.value).toBe("draft in progress")

    app.composer.value = "top line\nbottom line"
    app.composer.editor.gotoBufferEnd()
    setup.mockInput.pressArrow("up")
    expect(app.composer.value).toBe("top line\nbottom line")
    expect(app.composer.editor.logicalCursor.row).toBe(0)
  })

  test("restores the unsent draft after cycling slash-command history", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      onCommand() {
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.composer.value = "/status"
    expect(await app.composer.submit()).toBeTrue()
    app.composer.value = "/cost"
    expect(await app.composer.submit()).toBeTrue()
    app.composer.value = "draft in progress"
    app.composer.focus()

    setup.mockInput.pressArrow("up")
    expect(app.composer.value).toBe("/cost")
    expect(app.picker.visible).toBeFalse()
    setup.mockInput.pressArrow("up")
    expect(app.composer.value).toBe("/status")
    expect(app.picker.visible).toBeFalse()
    // Production OpenTUI can publish a deferred/duplicate content notification
    // after programmatic history restoration. It must not erase the cursor and
    // trap Down on the recalled command.
    app.composer.editor.setText(app.composer.value)
    setup.mockInput.pressArrow("down")
    expect(app.composer.value).toBe("/cost")
    expect(app.picker.visible).toBeFalse()
    setup.mockInput.pressArrow("down")
    expect(app.composer.value).toBe("draft in progress")
    expect(app.picker.visible).toBeFalse()
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

  test("leaves insert mode before double Escape interrupts", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      keybindings: { preset: "vim" },
      initialState: {
        ...createInitialState(),
        turns: { active: { turnId: "active", status: "running", usage: null, cost: null } },
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    setup.mockInput.pressKey("i")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.statusLine.plainText).toContain("NORMAL")
    expect(app.banner.plainText).toContain("Esc again")
    expect(commands).toHaveLength(0)
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(commands.at(-1)).toMatchObject({ type: "interrupt" })
  })

  test("keeps double Escape available while a safety panel owns focus", async () => {
    const setup = await createTestRenderer({ width: 100, height: 28, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        turns: { active: { turnId: "active", status: "running", usage: null, cost: null } },
        tools: {
          approval: {
            toolCallId: "approval",
            turnId: "active",
            name: "bash",
            args: { command: "cargo test" },
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
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()
    expect(app.interactionPanel.capturesInput).toBeTrue()

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.banner.plainText).toContain("Esc again")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(commands.at(-1)).toMatchObject({ type: "interrupt" })
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
