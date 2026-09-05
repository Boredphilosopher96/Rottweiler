import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, EngineEvent } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptyHistoryReader } from "../fixtures/history"

describe("Rottweiler providers-sessions", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("quick-connects fresh built-in providers through connection-scoped auth prompts", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const openedUrls: string[] = []
    const copiedText: string[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        providers: [
          {
            name: "github_copilot",
            authKind: "device_flow",
            nextAction: "configure",
            configured: false,
            authenticated: false,
            reachable: false,
            modelCount: 0,
            status: "setup required",
          },
          {
            name: "openai_codex",
            authKind: "oauth",
            nextAction: "configure",
            configured: false,
            authenticated: false,
            reachable: false,
            modelCount: 0,
            status: "setup required",
          },
        ],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
      externalUrl: {
        async open(url) {
          openedUrls.push(url)
        }
      },
      textClipboard: {
        async writeText(value) {
          copiedText.push(value)
        }
      }
    })
    renderer.root.add(app)

    app.openProviderPicker()
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "configure_builtin_provider",
      provider: "github_copilot",
    }))
    app.handleEvent({
      type: "provider_configured",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "configure-copilot",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      provider: "github_copilot",
      auth_kind: "device_flow",
    })
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "begin_provider_auth",
      provider: "github_copilot",
    }))
    app.handleEvent({
      type: "provider_auth_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "begin-copilot",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      attempt_id: "attempt-1",
      provider: "github_copilot",
      challenge: {
        kind: "device_flow",
        verification_uri: "https://github.com/login/device",
        user_code: "ABCD-1234",
      },
      warnings: [],
    })
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "complete_provider_auth",
      provider: "github_copilot",
      attempt_id: "attempt-1",
    }))
    expect(app.picker.title).toContain("Sign in · GitHub Copilot")
    expect(app.picker.select.options[0]?.description).toContain("ABCD-1234")
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "provider-auth.open",
      "provider-auth.copy-code",
      "provider-auth.copy-url",
      "provider-auth.cancel",
    ])
    app.handleEvent({
      type: "provider_auth_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "begin-copilot-replayed",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "session-local",
      attempt_id: "attempt-1",
      provider: "github_copilot",
      challenge: {
        kind: "device_flow",
        verification_uri: "https://github.com/login/device",
        user_code: "ABCD-1234",
      },
      warnings: [],
    })
    expect(emitted.filter((command) =>
      command.type === "complete_provider_auth" && command.attempt_id === "attempt-1"
    )).toHaveLength(1)
    await Bun.sleep(0)
    expect(openedUrls).toEqual(["https://github.com/login/device"])
    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(openedUrls).toEqual([
      "https://github.com/login/device",
      "https://github.com/login/device",
    ])

    app.picker.select.setSelectedIndex(1)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(copiedText).toEqual(["ABCD-1234"])

    app.picker.select.setSelectedIndex(2)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(copiedText).toEqual(["ABCD-1234", "https://github.com/login/device"])
    expect(app.state.providerAuth.pending?.challenge).toEqual({
      kind: "device_flow",
      verification_uri: "https://github.com/login/device",
      user_code: "ABCD-1234",
    })

    const refreshesBeforeAuthFinished = emitted.filter(
      (command) => command.type === "list_models" && command.refresh,
    ).length
    app.handleEvent({
      type: "provider_auth_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "complete-copilot",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      attempt_id: "attempt-1",
      provider: "github_copilot",
      success: true,
      message: "provider authentication completed",
      warnings: [],
    })
    expect(emitted.filter(
      (command) => command.type === "list_models" && command.refresh,
    )).toHaveLength(refreshesBeforeAuthFinished)
    app.handleEvent({
      type: "provider_activation_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "complete-copilot",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "session-local",
      provider: "github_copilot",
      success: true,
      message: "Provider connected. Choose a model from /models.",
    })
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "list_models",
      refresh: true,
    }))
    expect(emitted.filter(
      (command) => command.type === "list_models" && command.refresh,
    )).toHaveLength(refreshesBeforeAuthFinished + 1)

    app.openProviderPicker()
    const codex = app.picker.select.options.findIndex((option) => option.value === "openai_codex")
    app.picker.select.setSelectedIndex(codex)
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "configure_builtin_provider",
      provider: "openai_codex",
    }))
  })

  test("keeps OpenAI API distinct from ChatGPT and shows session workspaces", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        providers: [{
          name: "openai",
          authKind: "oauth",
          nextAction: "authenticate",
          configured: true,
          authenticated: false,
          reachable: false,
          modelCount: 0,
          status: null,
        }],
        sessions: [{
          sessionId: "session-workspace",
          title: "Fix login",
          workspaceName: "payments-service",
          model: "gpt-5",
          driverClientId: null,
          shellActive: false,
        }],
      },
      onCommand: () => ({ type: "accepted" }),
    })
    renderer.root.add(app)

    app.openProviderPicker()
    expect(app.picker.select.options[0]?.name).toBe("OpenAI API")
    expect(app.picker.select.options[0]?.name).not.toContain("ChatGPT")
    expect(app.picker.select.options[0]?.description).not.toContain("ChatGPT")
    app.openSessionPicker()
    expect(app.picker.select.options[0]?.name).toBe("New session")
    expect(app.picker.select.options[1]?.name).toBe("Fix login")
    expect(app.picker.select.options[1]?.description).toContain("payments-service")
  })

  test("creates a clean session from Ctrl-N and switches only after correlated acceptance", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const selected: string[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      requestId: () => "new-session-request",
      initialState: {
        ...createInitialState(),
        workspaceRoots: {
          generation: "1",
          effectiveFromTurn: "0",
          roots: ["/workspace/project"],
        },
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      onSessionSelect(sessionId) {
        selected.push(sessionId)
      },
    })
    renderer.root.add(app)

    setup.mockInput.pressKey("n", { ctrl: true })
    await Bun.sleep(0)
    expect(commands).toContainEqual(expect.objectContaining({
      type: "create_session",
      cwd: "/workspace/project",
      model: null,
    }))
    expect(selected).toEqual([])

    app.handleEvent({
      type: "command_acknowledged",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "new-session-request",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "new-session",
      outcome: { type: "accepted" },
    })
    expect(selected).toEqual(["new-session"])
  })

  test("renames a listed session through per-row actions without switching", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const selected: string[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "active-session",
      initialState: {
        ...createInitialState(),
        sessions: [{
          sessionId: "past-session",
          title: "Fix login",
          workspaceName: "payments-service",
          model: "fast",
          driverClientId: null,
          shellActive: false,
        }],
      },
      requestId: () => `rename-${++request}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      onSessionSelect(sessionId) {
        selected.push(sessionId)
      },
    })
    renderer.root.add(app)

    app.openSessionPicker()
    app.picker.select.setSelectedIndex(1)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Session actions · Fix login")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "Resume session",
      "Rename session",
    ])
    app.picker.select.setSelectedIndex(1)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Rename session, e.g. Auth refactor")
    expect(app.picker.input.value).toBe("")
    expect(app.picker.input.placeholder).toBe("Fix login")

    await setup.mockInput.typeText("Auth refactor")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(selected).toEqual([])
    expect(commands).toContainEqual(expect.objectContaining({
      type: "rename_session",
      session_id: "past-session",
      title: "Auth refactor",
    }))

    app.handleEvent({
      type: "session_title_updated",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "past-session",
        sequence_id: "7",
        emitted_at: "2026-01-01T00:00:00Z",
        caused_by: "rename-2",
      },
      title: "Auth refactor",
    })
    expect(app.picker.title).toContain("Sessions")
    expect(app.picker.select.options[1]?.name).toBe("Auth refactor")
    expect(app.state.sessions[0]?.title).toBe("Auth refactor")
    expect(app.state.lastSequence).toBeNull()
    expect(selected).toEqual([])
  })

  test("offers activation retry and credential replacement for unreachable providers", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const activations: string[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        providers: [{
          name: "openai_codex",
          authKind: "oauth",
          nextAction: "select_models",
          configured: true,
          authenticated: true,
          reachable: false,
          modelCount: 0,
          status: "provider model discovery rejected the stored credential",
        }],
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      async onProviderActivate(provider) {
        activations.push(provider)
      },
    })
    renderer.root.add(app)

    app.openProviderPicker()
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("OpenAI · ChatGPT")
    expect(app.picker.title).not.toContain("openai_codex")
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "provider-recovery.activate",
      "provider-recovery.reauthenticate",
    ])
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(activations).toEqual(["openai_codex"])

    app.openProviderPicker()
    app.picker.select.selectCurrent()
    const reauthenticate = app.picker.select.options.findIndex(
      (option) => option.value === "provider-recovery.reauthenticate",
    )
    app.picker.select.setSelectedIndex(reauthenticate)
    app.picker.select.selectCurrent()
    expect(commands).toContainEqual(expect.objectContaining({
      type: "begin_provider_auth",
      provider: "openai_codex",
    }))
  })

  test("offers OAuth browser and URL actions with sanitized adapter failures", async () => {
    const setup = await createTestRenderer({
      width: 100,
      height: 24,
      useThread: false,
    })
    renderer = setup.renderer
    const copied: string[] = []
    const authorizationUrl =
      "https://auth.example.test/authorize?state=challenge-canary"
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand() {
        return { type: "accepted" }
      },
      externalUrl: {
        async open() {
          throw new Error(`launcher leaked ${authorizationUrl}`)
        },
      },
      textClipboard: {
        async writeText(value) {
          copied.push(value)
        },
      },
    })
    renderer.root.add(app)
    app.handleEvent({
      type: "provider_auth_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "begin-codex",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      attempt_id: "attempt-oauth",
      provider: "openai_codex",
      challenge: {
        kind: "oauth",
        authorization_url: authorizationUrl,
        redirect_uri: "http://127.0.0.1:1455/callback",
      },
      warnings: [],
    })

    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "provider-auth.open",
      "provider-auth.copy-url",
      "provider-auth.cancel",
    ])
    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const error = app.state.errors.at(-1)
    expect(error?.code).toBe("provider_auth_browser_failed")
    expect(error?.message).toContain("Copy URL")
    expect(error?.message).not.toContain("challenge-canary")
    expect(error?.message).not.toContain("launcher leaked")

    const copyUrl = app.picker.select.options.findIndex(
      (option) => option.value === "provider-auth.copy-url",
    )
    app.picker.select.setSelectedIndex(copyUrl)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(copied).toEqual([authorizationUrl])
    expect(
      app.picker.select.options.find((option) => option.value === "provider-auth.open")
        ?.description,
    ).toContain("URL copied")
  })

  test("masks and clears non-protocol provider API keys, including custom providers", async () => {
    const setup = await createTestRenderer({
      width: 100,
      height: 24,
      useThread: false
    })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const submissions: Array<{ provider: string; apiKey: string }> = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      async onProviderApiKey(provider, apiKey) {
        submissions.push({ provider, apiKey })
        return { stored: true, activated: false, warnings: [] }
      }
    })
    renderer.root.add(app)
    const canary = "rw-secret-canary-tui"
    app.openProviderApiKeyPrompt("company-openai")
    await setup.mockInput.typeText(canary)
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain(canary)
    expect(setup.captureCharFrame()).toContain("•".repeat(canary.length))
    expect(JSON.stringify(app.state)).not.toContain(canary)
    expect(JSON.stringify(commands)).not.toContain(canary)

    setup.mockInput.pressEnter()
    await Bun.sleep(10)
    expect(submissions).toEqual([
      { provider: "company-openai", apiKey: canary }
    ])
    expect(app.picker.input.value).toBe("")
    expect(app.state.errors.at(-1)?.code).toBe("provider_activation_pending")
    expect(JSON.stringify(app.state)).not.toContain(canary)
  })

  test("surfaces a correlated rejected model switch as a bounded visible error", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const priorErrors = Array.from({ length: 64 }, (_, index) => ({
      category: "protocol" as const,
      code: `prior-${index}`,
      message: `Prior error ${index}`,
      retryable: false,
    }))
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      requestId: () => "rejected-model-switch",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        errors: priorErrors,
        models: [{
          id: "openai/fast",
          displayName: "fast",
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
    })
    renderer.root.add(app)
    app.openModelPicker()
    app.picker.select.selectCurrent()
    app.handleEvent({
      type: "command_acknowledged",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "rejected-model-switch",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      outcome: {
        type: "rejected",
        error: {
          category: "protocol",
          code: "session_not_idle",
          message: "model switching requires an idle session",
          retryable: true,
        },
      },
    })

    expect(commands).toContainEqual(expect.objectContaining({ type: "switch_model", model: "openai/fast" }))
    expect(app.state.errors).toHaveLength(64)
    expect(app.state.errors.at(-1)?.code).toBe("session_not_idle")
    expect(app.banner.visible).toBeTrue()
    expect(app.banner.plainText).toContain("model switching requires an idle session")
    expect(commands).not.toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "project.models.default",
    }))
  })

  test("leaves accepted model persistence to the host transaction", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      requestId: () => `model-correlation-${request++}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        models: [{
          id: "openai/fast",
          displayName: "fast",
          provider: "openai",
          aliases: ["fast"],
          current: false,
          available: true,
          status: null,
          vision: false,
          thinking: true,
          toolCalling: true,
        }],
      },
    })
    renderer.root.add(app)
    for (let index = 0; index < 130; index += 1) {
      app.openModelPicker()
      app.picker.select.selectCurrent()
    }
    const switches = commands.filter((command) => command.type === "switch_model")
    expect(switches).toHaveLength(130)
    const lastRequest = switches.at(-1)?.meta.request_id
    app.handleEvent({
      type: "model_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
        caused_by: lastRequest ?? null,
      },
      model: "fast",
    })
    const persisted = commands.filter(
      (command) => command.type === "set_setting" && command.key === "project.models.default",
    )
    expect(persisted).toHaveLength(0)
  })
})
