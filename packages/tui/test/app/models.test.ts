import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"

describe("Rottweiler models", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("offers exact model-provider route switching through typed pickers", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        models: [
          { id: "openai/fast", displayName: "fast", provider: "openai", aliases: ["fast"], current: false, available: true, status: null, vision: true, thinking: true, toolCalling: true },
          { id: "copilot/steady", displayName: "steady", provider: "copilot", aliases: ["steady"], current: false, available: true, status: null, vision: false, thinking: true, toolCalling: true },
        ],
        providers: [
          { name: "copilot", authKind: "device_flow", nextAction: "select_models", configured: true, authenticated: true, reachable: true, modelCount: 1, status: null },
          { name: "openai", authKind: "api_key", nextAction: "select_models", configured: true, authenticated: true, reachable: true, modelCount: 1, status: null },
        ],
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.composer.value = "/providers"
    expect(await app.composer.submit()).toBeTrue()
    expect(app.picker.title).toContain("Providers")
    expect(app.picker.select.options.map((option) => option.value)).toEqual(["copilot", "openai"])
    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Models · copilot")
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "models.section.models",
      "copilot/steady",
    ])
    app.picker.select.setSelectedIndex(1)
    app.picker.select.selectCurrent()
    expect(commands).toContainEqual(expect.objectContaining({
      type: "switch_model",
      model: "copilot/steady",
      provider: "copilot",
    }))
    app.handleEvent({
      type: "model_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      model: "steady",
      provider: "copilot",
    })
    expect(app.state.provider).toBe("copilot")
    expect(app.statusLine.plainText).toContain("steady")

    app.composer.value = "/models"
    expect(await app.composer.submit()).toBeTrue()
    expect(app.picker.title).toContain("Models")
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "models.section.models",
      "openai/fast",
      "copilot/steady",
    ])
  })

  test("keeps failover aliases distinct from pinned model routes", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        models: [
          {
            id: "openai/gpt-5",
            displayName: "GPT-5",
            provider: "openai",
            aliases: ["fast"],
            current: true,
            available: true,
            status: null,
            vision: true,
            thinking: true,
            toolCalling: true,
          },
          {
            id: "anthropic/claude",
            displayName: "Claude",
            provider: "anthropic",
            aliases: ["fast"],
            current: false,
            available: true,
            status: null,
            vision: false,
            thinking: true,
            toolCalling: true,
          },
          {
            id: "offline/one",
            displayName: "Offline one",
            provider: "offline",
            aliases: [],
            current: false,
            available: false,
            status: null,
            vision: false,
            thinking: false,
            toolCalling: true,
          },
          {
            id: "offline/two",
            displayName: "Offline two",
            provider: "offline",
            aliases: [],
            current: false,
            available: false,
            status: null,
            vision: false,
            thinking: false,
            toolCalling: true,
          },
        ],
        modelAliases: [
          { alias: "fast", candidates: ["openai/gpt-5", "anthropic/claude"], current: true },
          { alias: "openai/gpt-5", candidates: ["openai/gpt-5"], current: false },
          { alias: "preferred", candidates: ["openai/gpt-5"], current: false },
          { alias: "offline", candidates: ["offline/one", "offline/two"], current: false },
        ],
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openModelPicker()
    const options = app.picker.select.options
    const values = options.map((option) => option.value)
    expect(values).toEqual([
      "models.section.failover-chains",
      "model-alias:fast",
      "model-alias:preferred",
      "model-alias:offline",
      "models.section.models",
      "openai/gpt-5",
      "anthropic/claude",
      "offline/one",
      "offline/two",
    ])
    expect(options[0]).toMatchObject({ name: "", description: "Failover chains" })
    expect(options[1]).toMatchObject({ name: "● fast", description: "failover · openai/gpt-5 → anthropic/claude · available" })
    expect(options[3]?.description).toContain("no available route")
    expect(options[5]?.description).toContain("pinned route")
    expect(options[4]).toMatchObject({ name: "", description: "Models" })

    app.picker.select.setSelectedIndex(values.indexOf("model-alias:fast"))
    app.picker.select.selectCurrent()
    const aliasSwitch = commands.find(
      (command) => command.type === "switch_model" && command.model === "fast",
    )
    expect(aliasSwitch).toMatchObject({ type: "switch_model", model: "fast" })
    expect(aliasSwitch).not.toHaveProperty("provider")

    app.openModelPicker()
    const offlineIndex = app.picker.select.options.findIndex(
      (option) => option.value === "model-alias:offline",
    )
    app.picker.select.setSelectedIndex(offlineIndex)
    app.picker.select.selectCurrent()
    const offlineSwitch = commands.find(
      (command) => command.type === "switch_model" && command.model === "offline",
    )
    expect(offlineSwitch).toMatchObject({ type: "switch_model", model: "offline" })
    expect(offlineSwitch).not.toHaveProperty("provider")

    app.openModelPicker()
    await setup.mockInput.typeText("fast")
    expect(app.picker.select.options.map((option) => option.value)).not.toContain(
      "models.section.failover-chains",
    )
    expect(app.picker.select.options.map((option) => option.value)).not.toContain(
      "models.section.models",
    )
  })

  test("clicking a model presents the three typed context choices with summary selected", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      sessionId: "session-model-context",
      requestId: () => `model-context-${request++}`,
      initialState: {
        ...createInitialState(),
        models: [{
          id: "openai/gpt-5",
          displayName: "GPT-5",
          provider: "openai",
          aliases: ["fast"],
          current: false,
          available: true,
          status: null,
          vision: true,
          thinking: true,
          toolCalling: true,
        }],
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openModelPicker()
    await setup.renderOnce()

    await setup.mockMouse.click(app.picker.select.x + 2, app.picker.select.y + 2)
    expect(commands).toContainEqual(expect.objectContaining({
      type: "switch_model",
      session_id: "session-model-context",
      model: "openai/gpt-5",
      provider: "openai",
    }))

    app.handleEvent({
      type: "question_asked",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-model-context",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      turn_id: "4",
      question_id: "model-switch-1",
      question: {
        id: "model-switch-1",
        prompt: "How should the new model receive this conversation?",
        response_kind: "select_one",
        model_switch: { model: "openai/gpt-5", provider: "openai" },
        options: [
          {
            value: "pass_summary",
            label: "Pass summary",
            description: "Compact this conversation, then switch models",
            model_context_transfer: "pass_summary",
          },
          {
            value: "pass_full_context",
            label: "Pass full context",
            description: "Switch models with the complete current history",
            model_context_transfer: "pass_full_context",
          },
          {
            value: "start_without_context",
            label: "Start without context",
            description: "Keep project instructions but start a fresh conversation",
            model_context_transfer: "start_without_context",
          },
        ],
      },
    })
    await setup.renderOnce()

    expect(app.interactionPanel.select.options.map((option) => option.value)).toEqual([
      "pass_summary",
      "pass_full_context",
      "start_without_context",
    ])
    expect(app.interactionPanel.select.getSelectedIndex()).toBe(0)
    expect(app.interactionPanel.select.getSelectedOption()?.name).toBe("Pass summary")
    setup.mockInput.pressEnter()
    expect(commands).toContainEqual(expect.objectContaining({
      type: "answer_question",
      session_id: "session-model-context",
      question_id: "model-switch-1",
      answer: {
        question_id: "model-switch-1",
        value: "pass_summary",
      },
    }))
  })

  test("uses provider inventory, concrete models, command sources, and persisted settings", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        commands: [{
          name: "deploy",
          description: "Deploy project",
          usage: "/deploy",
          source: "project",
        }],
        models: [{
          id: "copilot/gpt-5",
          displayName: "GPT-5",
          provider: "copilot",
          aliases: ["fast"],
          current: true,
          available: true,
          status: null,
          vision: true,
          thinking: true,
          toolCalling: true,
        }],
        providers: [{
          name: "copilot",
          authKind: "device_flow",
          nextAction: "select_models",
          configured: true,
          authenticated: false,
          reachable: false,
          modelCount: 0,
          status: "login required",
        }],
        settings: [
          {
            key: "ui.theme",
            label: "Theme",
            value: "opencode",
            choices: ["system", "opencode", "tokyonight"],
            provenance: "built-in",
            appliesImmediately: false,
          },
          {
            key: "models.thinking.fast",
            label: "Thinking · fast",
            value: "medium",
            choices: ["off", "low", "medium", "high"],
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "permissions.default",
            label: "Default permission",
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
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "ui.keybindings.preset",
            label: "Keybinding preset",
            value: "standard",
            choices: ["standard", "vim"],
            provenance: "user keybindings",
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
        ],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openProviderPicker()
    expect(app.picker.select.options.map((option) => option.value)).toEqual(["copilot"])
    expect(app.picker.select.options[0]?.description).toContain("Sign in with GitHub")

    app.openModelPicker()
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "models.section.models",
      "copilot/gpt-5",
    ])
    expect(app.picker.select.options.map((option) => option.name).join(" ")).not.toContain(
      "Alias ·",
    )
    expect(app.picker.select.options.map((option) => option.name).join(" ")).not.toContain(
      "Thinking ·",
    )
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "switch_model",
      model: "copilot/gpt-5",
      provider: "copilot",
    }))
    const concreteSwitch = emitted.find(
      (command) => command.type === "switch_model" && command.model === "copilot/gpt-5",
    )
    expect(concreteSwitch).toBeDefined()
    app.handleEvent({
      type: "model_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
        caused_by: concreteSwitch?.meta.request_id ?? null,
      },
      model: "copilot/gpt-5",
      provider: "copilot",
    })
    expect(app.statusLine.plainText).toContain("gpt-5")
    expect(app.statusLine.plainText).not.toContain("copilot/copilot")
    app.handleEvent({
      type: "conversation_turn_committed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z"
      },
      agent_turn: "1",
      turn: {
        role: "assistant",
        blocks: [{ type: "text", text: "fallback response" }],
        meta: {
          model: "openai/gpt-5-fallback",
          synthetic: false,
          summary: false
        }
      }
    })
    expect(app.state.provider).toBe("openai")
    expect(app.state.model).toBe("openai/gpt-5-fallback")
    expect(app.statusLine.plainText).toContain("gpt-5-fallback")
    expect(app.statusLine.plainText).not.toContain("openai/openai")
    expect(emitted).not.toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "project.models.default",
    }))

    expect(app.picker.input.isDestroyed).toBeFalse()
    app.openSettingsPicker()
    const settingOptions = app.settingsBrowser.itemIds
    expect(settingOptions).toContain("models.thinking.fast")
    expect(settingOptions).toContain("permissions.default")
    expect(settingOptions).toContain("compaction.auto")
    expect(settingOptions).toContain("ui.keybindings.preset")
    expect(settingOptions).toContain("mcp.servers.docs.enabled")
    app.settingsBrowser.selectById("ui.theme")
    app.settingsBrowser.activateSelected()
    app.themeBrowser.selectById("theme:tokyonight")
    app.themeBrowser.activateSelected()
    await Bun.sleep(10)
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "ui.theme",
      value: "tokyonight",
    }))

    app.closePicker()
    await setup.mockInput.typeText("/")
    const projectCommand = app.picker.select.options.find(
      (option) => option.value === "deploy",
    )
    expect(projectCommand?.description).toContain("Project · Deploy project")
    app.closePicker()
    app.openCommandPicker()
    app.commandPalette.selectById("slash.deploy")
    expect(app.commandPalette.detail.plainText).toContain("Commands · project")
    expect(app.commandPalette.detail.plainText).toContain("Deploy project")
  })
})
