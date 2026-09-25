import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, CommandOutcome, EngineEvent } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"
import { options, select, statusText } from "../picker-screen"

describe("Rottweiler discovery", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })
  test.each([
    { discovered: false, catalogFirst: false },
    { discovered: false, catalogFirst: true },
    { discovered: true, catalogFirst: false },
    { discovered: true, catalogFirst: true },
  ])("resumes setup for a configured provider with no selected model across asynchronous projections (%p)", async ({ discovered, catalogFirst }) => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
      },
      onCommand(command) { emitted.push(command); return { type: "accepted" } },
    })
    renderer.root.add(app)

    const sessions = {
      type: "sessions_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "ready-sessions", emitted_at: "2026-01-01T00:00:00Z" },
      sessions: [],
    } satisfies EngineEvent
    const catalog = {
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "ready-models", emitted_at: "2026-01-01T00:00:01Z" },
      models: [],
      aliases: [],
      cached: !discovered,
      truncated: false,
      providers: [{
        name: "openai",
        auth_kind: "api_key",
        next_action: discovered ? "select_models" : "configure",
        configured: true,
        authenticated: discovered,
        reachable: discovered,
        model_count: discovered ? 1 : 0,
      }],
    } satisfies EngineEvent
    for (const event of catalogFirst ? [catalog, sessions] : [sessions, catalog]) {
      app.handleEvent(event)
      await Promise.resolve()
    }
    if (!discovered) {
      // A cached catalog cannot prove the provider is unusable; setup waits
      // for the live catalog instead of interrupting with a picker.
      expect(app.picker.visible).toBeFalse()
      expect(emitted).toContainEqual(expect.objectContaining({ type: "list_models", refresh: true }))
      const refresh = emitted.findLast(command => command.type === "list_models")
      app.handleEvent({ ...catalog, meta: { ...catalog.meta, request_id: refresh?.meta.request_id ?? "" }, cached: false })
      await Promise.resolve()
    }
    expect(app.picker.visible).toBeTrue()
    app.closePicker()
    await setup.mockInput.typeText("composer owns this")
    expect(app.composer.value).toBe("composer owns this")
  })

  test("a fresh session selects the first available tool-capable model of a connected provider", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: { ...createInitialState(), model: "fast", connection: { phase: "connected", attempt: 0, error: null, gap: null } },
      onCommand(command) { emitted.push(command); return { type: "accepted" } },
    })
    renderer.root.add(app)
    const meta = (request_id: string) => ({ protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id, emitted_at: "2026-01-01T00:00:00Z" })
    const capabilities = (tool_calling: boolean) => ({ tool_calling, vision: false, thinking: true, cache_behavior: "provider_managed" as const, max_context_tokens: "272000", max_output_tokens: null })
    const provider = { name: "openai_codex", auth_kind: "oauth" as const, next_action: "select_models" as const, configured: true, authenticated: true, reachable: true, model_count: 3 }
    const catalog = (cached: boolean) => ({
      type: "models_listed", meta: meta(cached ? "cached" : "live"), aliases: [], cached, truncated: false, providers: [provider],
      models: [
        { id: "openai_codex/gpt-image", display_name: "GPT Image", provider: "openai_codex", aliases: [], current: false, available: true, capabilities: capabilities(false) },
        { id: "openai_codex/gpt-5.6-terra", display_name: "GPT-5.6-Terra", provider: "openai_codex", aliases: [], current: false, available: true, capabilities: capabilities(true) },
        { id: "openai_codex/gpt-5.6-sol", display_name: "GPT-5.6-Sol", provider: "openai_codex", aliases: [], current: false, available: true, capabilities: capabilities(true) },
      ],
    }) satisfies EngineEvent
    app.handleEvent({ type: "sessions_listed", meta: meta("sessions"), sessions: [] })
    app.handleEvent(catalog(true))
    expect(app.statusLine.plainText).toContain("loading models")
    expect(app.statusLine.plainText).not.toContain("fast")
    expect(emitted.some(command => command.type === "switch_model")).toBeFalse()
    const refresh = emitted.findLast(command => command.type === "list_models")
    expect(refresh).toMatchObject({ refresh: true })
    app.handleEvent({ ...catalog(false), meta: meta(refresh?.meta.request_id ?? "") })
    expect(emitted).toContainEqual(expect.objectContaining({ type: "switch_model", model: "openai_codex/gpt-5.6-terra", provider: "openai_codex" }))
    expect(app.picker.visible).toBeFalse()
    app.handleEvent({ type: "model_changed", meta: { protocol_version: PROTOCOL_VERSION, session_id: "session-local", sequence_id: "1", emitted_at: "2026-01-01T00:00:02Z", caused_by: null }, model: "openai_codex/gpt-5.6-terra", provider: "openai_codex" } as EngineEvent)
    expect(app.statusLine.plainText).toContain("GPT-5.6-Terra")
    expect(app.statusLine.plainText).not.toContain("openai_codex/")
  })

  test.each([true, false])("a failed live refresh at launch falls back to the cached catalog (cached model available: %p)", async (cachedAvailable) => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: { ...createInitialState(), connection: { phase: "connected", attempt: 0, error: null, gap: null } },
      onCommand(command) {
        emitted.push(command)
        return command.type === "list_models" && command.refresh
          ? { type: "rejected", error: { category: "provider", code: "catalog_unavailable", message: "model catalog is offline", retryable: true } }
          : { type: "accepted" }
      },
    })
    renderer.root.add(app)
    const meta = (request_id: string) => ({ protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id, emitted_at: "2026-01-01T00:00:00Z" })
    const provider = { name: "openai_codex", auth_kind: "oauth" as const, next_action: "select_models" as const, configured: true, authenticated: true, reachable: true, model_count: 1 }
    app.handleEvent({ type: "sessions_listed", meta: meta("sessions"), sessions: [] })
    app.handleEvent({
      type: "models_listed", meta: meta("cached"), aliases: [], cached: true, truncated: false, providers: [provider],
      models: [{ id: "openai_codex/gpt-5.6-terra", display_name: "GPT-5.6-Terra", provider: "openai_codex", aliases: [], current: false, available: cachedAvailable,
        capabilities: { tool_calling: true, vision: false, thinking: true, cache_behavior: "provider_managed", max_context_tokens: "272000", max_output_tokens: null } }],
    })
    expect(emitted).toContainEqual(expect.objectContaining({ type: "list_models", refresh: true }))
    await Bun.sleep(0)
    if (cachedAvailable) {
      expect(emitted).toContainEqual(expect.objectContaining({ type: "switch_model", model: "openai_codex/gpt-5.6-terra" }))
      expect(app.picker.visible).toBeFalse()
    } else {
      expect(emitted.some(command => command.type === "switch_model")).toBeFalse()
      expect(app.picker.visible).toBeTrue()
      expect(options(app.picker).some(option => option.name.includes("Retry loading models"))).toBeTrue()
    }
  })

  test("keeps unavailable controls discoverable without dispatching them", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: { ...createInitialState(), connection: { phase: "connected", attempt: 0, error: null, gap: null } },
      onCommand(command) { emitted.push(command); return { type: "accepted" } },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    const reply = (reason: string | null) => {
      const request = emitted.findLast(command => command.type === "list_commands")
      if (request?.type !== "list_commands") throw new Error("missing catalog request")
      app.handleEvent({
        type: "command_descriptors_listed",
        meta: { protocol_version: PROTOCOL_VERSION, client_id: "tui", request_id: request.meta.request_id, emitted_at: "2026-01-01T00:00:00Z" },
        session_id: "session-local", commands: [], truncated: false,
        available_actions: [
          { action: "compact", unavailable_reason: reason },
          { action: "switch_mode", unavailable_reason: reason },
          { action: "switch_model", unavailable_reason: reason },
          { action: "rewind", unavailable_reason: reason },
          { action: "review", unavailable_reason: reason },
          { action: "fork", unavailable_reason: reason },
          { action: "add_workspace_root", unavailable_reason: reason },
        ],
      })
    }
    reply("Stop the current turn or wait for it to finish.")
    for (const id of ["cmd.compact", "cmd.mode", "cmd.model", "cmd.rewind", "cmd.review"]) {
      app.commandPalette.selectById(id)
      expect(app.commandPalette.activateSelected()).toBeFalse()
      expect(app.commandPalette.detail.plainText).toContain("Stop the current turn")
    }
    expect(emitted.filter(command => command.type === "send_message")).toHaveLength(0)
    app.closePicker()
    app.openCommandPicker()
    reply(null)
    app.commandPalette.selectById("cmd.compact")
    expect(app.commandPalette.activateSelected()).toBeTrue()
    await Bun.sleep(0)
    expect(emitted).toContainEqual(expect.objectContaining({ type: "send_message", content: "/compact" }))
  })

  test("searches settings actions and never one-clicks destructive choices", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        commands: [{ source: "builtin", scope: null, name: "mcp", description: "Manage MCP servers", usage: "[status]" }],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.mockInput.typeText("mcp")
    expect(app.commandPalette.itemIds).toContain("cmd.mcp")

    app.commandPalette.input.value = "folder trust"
    expect(app.commandPalette.itemIds[0]).toBe("cmd.permissions")
    app.closePicker()
    app.openTrustPicker()
    expect(app.picker.screenTitle).toContain("Folder trust")
    const grantIndex = options(app.picker).findIndex(
      (option) => option.value === "trust.grant",
    )
    select(app.picker, grantIndex)
    app.picker.activateSelected()
    await Bun.sleep(0)
    expect(app.composer.value).toBe("")
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "send_message",
      content: "/permissions trust grant",
    }))
  })

  test("refreshes live catalogs when pickers reopen and workspace roots change", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        commands: [{ source: "builtin", scope: null, name: "first", description: "First", usage: "" }],
        models: [{ id: "openai/fast", displayName: "fast", provider: "openai", aliases: ["fast"], current: false, available: true, status: null, vision: false, thinking: false, toolCalling: true, contextTokens: null }],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    const firstCatalogRequest = emitted.find((command) => command.type === "list_commands")
    expect(firstCatalogRequest?.type).toBe("list_commands")
    app.handleEvent({
      type: "command_descriptors_listed", available_actions: [],
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: firstCatalogRequest!.meta.request_id, emitted_at: "2026-01-01T00:00:00Z" },
      session_id: "session-local",
      commands: [{ source: "builtin", scope: null, name: "second", description: "Second", usage: "" }],
      truncated: false,
    })
    app.closePicker()
    app.openCommandPicker()
    expect(emitted.filter((command) => command.type === "list_commands")).toHaveLength(2)

    app.handleEvent({
      type: "command_finished",
      meta: { protocol_version: PROTOCOL_VERSION, session_id: "session-local", sequence_id: "1", emitted_at: "2026-01-01T00:00:01Z" },
      name: "add-dir",
      message: "added workspace root @root/1",
      unrestorable_paths: [],
    })
    expect(emitted.filter((command) => command.type === "list_commands")).toHaveLength(3)
    expect(emitted.filter((command) => command.type === "list_modes")).toHaveLength(1)

    app.openModePicker()
    const firstModesRequest = emitted.findLast((command) => command.type === "list_modes")
    expect(firstModesRequest?.type).toBe("list_modes")
    app.handleEvent({
      type: "modes_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: firstModesRequest!.meta.request_id, emitted_at: "2026-01-01T00:00:02Z" },
      session_id: "session-local",
      modes: [
        { id: "execute", description: "Make changes", current: true },
        { id: "audit", description: "Inspect controls and evidence", current: false },
      ],
      truncated: false,
    })
    expect(app.state.mode).toBe("execute")
    app.closePicker()
    app.openModePicker()
    const auditIndex = options(app.picker).findIndex((option) => option.value === "mode:audit")
    expect(auditIndex).toBeGreaterThanOrEqual(0)
    select(app.picker, auditIndex)
    app.picker.activateSelected()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "switch_mode",
      mode: "audit",
    }))
    app.handleEvent({ definition_fingerprint: "fixture",
      type: "mode_changed",
      meta: { protocol_version: PROTOCOL_VERSION, session_id: "session-local", sequence_id: "2", emitted_at: "2026-01-01T00:00:03Z" },
      mode: "audit",
    })
    expect(app.statusLine.plainText).toContain("AUDIT")
    app.openModePicker()
    const currentAudit = options(app.picker).find((option) => option.value === "mode:audit")
    expect(currentAudit?.name).toBe("Audit")
    expect(app.picker.selectedItem).toMatchObject({ id: "mode:audit", marker: "●" })
    app.closePicker()

    app.openModelPicker()
    const firstModelsRequest = emitted.find((command) => command.type === "list_models")
    expect(firstModelsRequest?.type).toBe("list_models")
    app.handleEvent({ aliases: [], cached: false, truncated: false,
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: firstModelsRequest!.meta.request_id, emitted_at: "2026-01-01T00:00:02Z" },
      models: [],
      providers: [],
    })
    app.closePicker()
    app.openModelPicker()
    expect(emitted.filter((command) => command.type === "list_models")).toHaveLength(2)
  })

  test("offers provider onboarding once when sessions arrive before the first unready model catalog", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
      },
    })
    renderer.root.add(app)

    app.handleEvent({
      type: "sessions_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "fresh-sessions", emitted_at: "2026-01-01T00:00:00Z" },
      sessions: [],
    })
    expect(app.picker.visible).toBeFalse()

    app.handleEvent({ aliases: [], cached: false, truncated: false,
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "first-models", emitted_at: "2026-01-01T00:00:01Z" },
      models: [],
      providers: [{
        name: "openai",
        auth_kind: "api_key",
        next_action: "configure",
        configured: false,
        authenticated: false,
        reachable: false,
        model_count: 0,
      }],
    })
    expect(app.picker.screenTitle).toContain("WELCOME   connect a provider to start")

    app.closePicker()
    app.handleEvent({ aliases: [], cached: false, truncated: false,
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "refreshed-models", emitted_at: "2026-01-01T00:00:02Z" },
      models: [],
      providers: [{
        name: "openai",
        auth_kind: "api_key",
        next_action: "configure",
        configured: false,
        authenticated: false,
        reachable: false,
        model_count: 0,
      }],
    })
    expect(app.picker.visible).toBeFalse()
  })

  test("does not revive an unresolved session alias after an empty catalog", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      sessionId: "session-restored",
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
      },
    })
    renderer.root.add(app)

    app.handleEvent({ aliases: [], cached: false, truncated: false,
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "premature-models", emitted_at: "2026-01-01T00:00:00Z" },
      models: [],
      providers: [],
    })
    expect(app.picker.visible).toBeFalse()

    app.handleEvent({
      type: "sessions_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "restored-session", emitted_at: "2026-01-01T00:00:01Z" },
      sessions: [{
        session_id: "session-restored",
        title: "Restored session",
        workspace_name: "Rottweiler",
        model: "fast",
        driver_client_id: "ui",
        shell_active: false,
      }],
    })
    expect(app.state.model).toBeNull()
    expect(app.picker.visible).toBeTrue()
  })

  test("clears an unresolved restored model and offers provider onboarding", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      sessionId: "session-restored",
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
      },
    })
    renderer.root.add(app)

    app.handleEvent({
      type: "sessions_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "restored-session", emitted_at: "2026-01-01T00:00:00Z" },
      sessions: [{
        session_id: "session-restored",
        title: "Restored session",
        workspace_name: "Rottweiler",
        model: "fast",
        driver_client_id: "ui",
        shell_active: false,
      }],
    })
    expect(app.state.model).toBe("fast")

    app.handleEvent({ aliases: [], cached: false, truncated: false,
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "reconnected-models", emitted_at: "2026-01-01T00:00:01Z" },
      models: [],
      providers: [],
    })
    expect(app.state.model).toBeNull()
    expect(app.picker.visible).toBeTrue()
  })

  test("does not interrupt a non-empty composer with provider onboarding", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
      },
    })
    renderer.root.add(app)
    app.composer.value = "already typing"

    app.handleEvent({
      type: "sessions_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "typed-sessions", emitted_at: "2026-01-01T00:00:00Z" },
      sessions: [],
    })
    app.handleEvent({ aliases: [], cached: false, truncated: false,
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "typed-models", emitted_at: "2026-01-01T00:00:01Z" },
      models: [],
      providers: [],
    })
    expect(app.picker.visible).toBeFalse()
  })

  test.each([{ count: 1, preserve: false }, { count: 5, preserve: false }, { count: 5, preserve: true }])("selects the first activation model while preserving an existing selection (%p)", async ({ count, preserve }) => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        model: preserve ? "openai/model-1" : null,
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.handleEvent({
      type: "provider_activation_finished",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: "activation", emitted_at: "2026-01-01T00:00:00Z" },
      session_id: "session-local",
      provider: "openai",
      success: true,
      message: "Connected",
    })
    const refresh = emitted.findLast((command) => command.type === "list_models")
    expect(refresh?.type).toBe("list_models")
    app.handleEvent({ aliases: [], cached: false, truncated: false,
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: refresh!.meta.request_id, emitted_at: "2026-01-01T00:00:01Z" },
      models: Array.from({ length: count }, (_, index) => ({
        id: index === 0 ? "openai/gpt-5" : `openai/model-${index}`,
        display_name: "GPT-5",
        provider: "openai",
        aliases: ["fast"],
        current: preserve && index === 1,
        available: true,
        capabilities: { vision: true, thinking: true, tool_calling: true, cache_behavior: "none", max_context_tokens: null, max_output_tokens: null },
      })),
      providers: [{
        name: "openai",
        auth_kind: "api_key",
        next_action: "select_models",
        configured: true,
        authenticated: true,
        reachable: true,
        model_count: 1,
      }],
    })
    if (preserve) {
      expect(emitted.some(command => command.type === "switch_model")).toBeFalse()
      expect(app.state.model).toBe("openai/model-1")
      return
    }
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "switch_model",
      model: "openai/gpt-5",
      provider: "openai",
    }))
    expect(app.picker.visible).toBeFalse()
  })

  test("shows command catalog truncation once without a palette pseudo-action", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    const request = emitted.find((command) => command.type === "list_commands")
    if (request?.type !== "list_commands") throw new Error("missing command catalog request")
    const event = {
      type: "command_descriptors_listed", available_actions: [],
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: request.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      commands: [{ source: "builtin", scope: null, name: "fixture", description: "Fixture", usage: "/fixture" }],
      truncated: true,
    } satisfies EngineEvent
    app.handleEvent(event)
    app.handleEvent(event)
    expect(app.state.errors.filter((error) => error.code === "command_catalog_truncated")).toHaveLength(1)
    expect(app.banner.plainText).toContain("command catalog is too large")
    expect(options(app.picker).map((option) => option.value)).not.toContain("commands.truncated")
    app.closePicker()
    await setup.mockInput.typeText("/")
    expect(app.slashPopup.visible).toBeTrue()
  })

  test("keeps local slash commands usable while a rejected live catalog is loud and retryable", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    let attempts = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      onCommand(command) {
        if (command.type !== "list_commands") return { type: "accepted" }
        attempts += 1
        return {
          type: "rejected",
          error: {
            category: "protocol",
            code: "catalog_unavailable",
            message: "driver lease rejected the command catalog",
            retryable: true,
          },
        }
      },
    })
    renderer.root.add(app)

    await setup.mockInput.typeText("/")
    await Bun.sleep(0)

    expect(app.slashPopup.itemIds).toContain("cmd.model")
    expect(app.banner.plainText).toContain("couldn't load commands")
    app.composer.value = ""
    app.openCommandPicker()
    app.commandPalette.selectById("ext.retry")
    app.commandPalette.activateSelected()
    await Bun.sleep(0)
    expect(attempts).toBe(2)
  })

  test("ignores late projection failures and engine events after OpenTUI destroys the application", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    let finishProjection!: (outcome: CommandOutcome) => void
    const deferredProjection = new Promise<CommandOutcome>((resolve) => {
      finishProjection = resolve
    })
    let postDestroyCommands = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      onCommand(command) {
        if (command.type === "list_commands") return deferredProjection
        postDestroyCommands += 1
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    renderer.destroy()
    renderer = undefined
    finishProjection({
      type: "rejected",
      error: {
        category: "protocol",
        code: "runtime_stopped",
        message: "the projection was cancelled during teardown",
        retryable: true,
      },
    })
    await Bun.sleep(0)

    expect(app.state.errors).toHaveLength(0)

    app.handleEvent({
      type: "command_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      name: "status",
      message: "actor idle · queue empty",
      unrestorable_paths: [],
    })
    expect("transcript" in app.state).toBe(false)
    expect(postDestroyCommands).toBe(0)
  })

  test("renders model projection failures in both model and provider pickers", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      onCommand(command) {
        if (command.type !== "list_models") return { type: "accepted" }
        return Promise.reject(new Error("provider discovery timed out"))
      },
    })
    renderer.root.add(app)

    app.openModelPicker()
    await Bun.sleep(0)
    expect(options(app.picker)[0]?.value).toBe("models.error")
    expect(options(app.picker)[0]?.description).toContain("provider discovery timed out")

    app.closePicker()
    app.openProviderPicker()
    await Bun.sleep(0)
    expect(options(app.picker)[0]?.value).toBe("providers.error")
    expect(options(app.picker)[0]?.description).toContain("provider discovery timed out")
  })

  test("presents model and provider loading as non-selectable picker status", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)

    app.openProviderPicker()
    expect(statusText(app.picker)).toContain("Loading provider connections")
    expect((app.picker.mode === "status")).toBeTrue()
    expect((app.picker.mode === "list")).toBeFalse()
    expect(options(app.picker)).toHaveLength(0)
    app.picker.activateSelected()
    expect(app.state.errors).toHaveLength(0)

    app.openModelPicker()
    expect(statusText(app.picker)).toContain("Loading available models")
    expect((app.picker.mode === "list")).toBeFalse()
    expect(options(app.picker)).toHaveLength(0)
    app.picker.activateSelected()
    expect(app.state.errors).toHaveLength(0)
  })

  test("presents loaded-empty file results and still offers a new session", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    let request = 0
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      requestId: () => `empty-${++request}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openFilePicker("missing")
    const files = commands.at(-1)
    if (files?.type !== "search_workspace_files") throw new Error("missing workspace search")
    app.handleEvent({
      type: "workspace_files_found",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: files.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      matches: [],
      truncated: false,
    })
    expect(statusText(app.picker)).toContain("No matching files")
    expect((app.picker.mode === "list")).toBeFalse()

    app.openSessionPicker()
    const sessions = commands.at(-1)
    if (sessions?.type !== "list_sessions") throw new Error("missing session list")
    app.handleEvent({
      type: "sessions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: sessions.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      sessions: [],
    })
    expect(options(app.picker)).toEqual([])
    expect((app.picker.mode === "list")).toBeTrue()
    expect(statusText(app.picker)).toContain("ctrl+n starts one in this workspace")
    expect(app.picker.footer.plainText).toBe("ctrl+x export · ctrl+n new · esc close")
  })
})
