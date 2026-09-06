import { describe, expect, test } from "bun:test"
import {
  PROTOCOL_VERSION
} from "../../src/protocol"
import {
  createInitialState,
  reduceRottweilerState,
  transportDisconnected
} from "../../src/state"
import { meta, reduce } from "./fixtures"

describe("state catalog", () => {

  test("starts without inventing an engine mode catalog", () => {
    expect(createInitialState().modes).toEqual([])
  })

  test("drops connection-scoped provider auth challenges on disconnect", () => {
    const pending = reduce(createInitialState(), {
      type: "provider_auth_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-auth",
        request_id: "request-auth",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-state",
      provider: "github_copilot",
      attempt_id: "attempt-auth",
      challenge: {
        kind: "device_flow",
        verification_uri: "https://github.com/login/device",
        user_code: "ABCD-EFGH",
      },
      warnings: [],
    })
    expect(pending.providerAuth.pending?.attemptId).toBe("attempt-auth")
    const disconnected = reduceRottweilerState(
      pending,
      transportDisconnected(1, "fixture disconnect"),
    )
    expect(disconnected.providerAuth.pending).toBeNull()
  })

  test("retains command catalog truncation so the UI cannot imply completeness", () => {
    const state = reduce(createInitialState(), {
      type: "command_descriptors_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "commands",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session",
      commands: [{ source: "builtin", name: "fixture", description: "Fixture", usage: "" }],
      truncated: true,
    })
    expect(state.commandsTruncated).toBeTrue()
  })

  test("projects a typed custom mode catalog and its completeness", () => {
    const state = reduce(createInitialState(), {
      type: "modes_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "modes",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session",
      modes: [
        { id: "execute", description: "Make changes", current: false },
        { id: "audit", description: "Inspect controls", current: true },
      ],
      truncated: true,
    })
    expect(state.modes).toEqual([
      { id: "execute", description: "Make changes", current: false },
      { id: "audit", description: "Inspect controls", current: true },
    ])
    expect(state.modesTruncated).toBeTrue()
    expect(state.mode).toBe("audit")
  })

  test("projects the unique concrete current model before the first turn", () => {
    const state = reduce(createInitialState(), { cached: false, truncated: false,
      type: "models_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-current",
        request_id: "request-current",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      models: [
        {
          id: "openai/gpt-5-mini",
          display_name: "GPT-5 mini",
          provider: "openai",
          aliases: ["fast"],
          current: true,
          available: true,
          capabilities: {
            tool_calling: true,
            vision: true,
            thinking: true,
            cache_behavior: "none",
            max_context_tokens: null,
            max_output_tokens: null,
          },
        },
      ],
      aliases: [{ alias: "fast", candidates: ["openai/gpt-5-mini"], current: true }],
      providers: [],
    })
    expect(state.model).toBe("openai/gpt-5-mini")
    expect(state.provider).toBe("openai")
  })

  test("projects the active session model when no model is set", () => {
    const withDriver = reduce(createInitialState(), {
      type: "driver_changed",
      meta: meta("1"),
      driver_client_id: "active-client",
    })
    const state = reduce(withDriver, {
      type: "sessions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "sessions",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      sessions: [
        { title: "Fixture",
          session_id: "other-session",
          workspace_name: "Rottweiler",
          model: "other-model",
          driver_client_id: "other-client",
          shell_active: false,
        },
        { title: "Fixture",
          session_id: "active-session",
          workspace_name: "Rottweiler",
          model: "active-model",
          driver_client_id: "active-client",
          shell_active: false,
        },
      ],
    })
    expect(state.model).toBe("active-model")
  })

  test("model catalog refresh does not overwrite a newer durable model event", () => {
    const durable = reduce(createInitialState(), {
      type: "model_changed",
      meta: meta("1"),
      model: "anthropic/claude-sonnet-4-5",
      provider: "anthropic",
    })
    const refreshed = reduce(durable, { cached: false, truncated: false,
      type: "models_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-current",
        request_id: "request-current",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      models: [
        {
          id: "openai/gpt-5-mini",
          display_name: "GPT-5 mini",
          provider: "openai",
          aliases: ["fast"],
          current: true,
          available: true,
          capabilities: {
            tool_calling: true,
            vision: true,
            thinking: true,
            cache_behavior: "none",
            max_context_tokens: null,
            max_output_tokens: null,
          },
        },
      ],
      aliases: [],
      providers: [],
    })
    expect(refreshed.model).toBe("anthropic/claude-sonnet-4-5")
    expect(refreshed.provider).toBe("anthropic")
  })

  test("an authoritative empty catalog clears a fresh unresolved session alias", () => {
    const withDriver = reduce(createInitialState(), {
      type: "driver_changed",
      meta: meta("1"),
      driver_client_id: "active-client",
    })
    const withDescriptor = reduce(withDriver, {
      type: "sessions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "sessions",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      sessions: [{ title: "Fixture",
        session_id: "fresh-session",
        workspace_name: "Rottweiler",
        model: "fast",
        driver_client_id: "active-client",
        shell_active: false,
      }],
    })
    expect(withDescriptor.model).toBe("fast")
    const resolved = reduce(withDescriptor, { cached: false, truncated: false,
      type: "models_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "models",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      models: [],
      aliases: [],
      providers: [],
    })
    expect(resolved.model).toBeNull()
    expect(resolved.provider).toBeNull()
    expect(resolved.modelCatalogLoaded).toBeTrue()
  })

  test("classifies model context clearing as a known durable event", () => {
    const state = reduce(createInitialState(), {
      type: "model_context_cleared",
      meta: meta("1"),
      strategy: "start_without_context",
    })

    expect(state.lastSequence).toBe("1")
    expect(state.protocol).toMatchObject({ invalidEvents: 0 })
  })

  test("preserves typed model-switch context choices for the interaction dock", () => {
    const state = reduce(createInitialState(), {
      type: "question_asked",
      meta: meta("1"),
      turn_id: "4",
      question_id: "model-switch-1",
      question: {
        id: "model-switch-1",
        prompt: "How should context move to the selected model?",
        response_kind: "select_one",
        model_switch: { model: "openai/gpt-5", provider: "openai" },
        options: [
          {
            value: "pass_summary",
            label: "Pass summary",
            model_context_transfer: "pass_summary",
          },
          {
            value: "pass_full_context",
            label: "Pass full context",
            model_context_transfer: "pass_full_context",
          },
          {
            value: "start_without_context",
            label: "Start without context",
            model_context_transfer: "start_without_context",
          },
        ],
      },
    })

    expect(state.questions["model-switch-1"]?.question).toMatchObject({
      model_switch: { model: "openai/gpt-5", provider: "openai" },
      options: [
        { value: "pass_summary", model_context_transfer: "pass_summary" },
        { value: "pass_full_context", model_context_transfer: "pass_full_context" },
        { value: "start_without_context", model_context_transfer: "start_without_context" },
      ],
    })
  })

  test("projects only the live runtime services returned by the host", () => {
    const state = reduce(createInitialState(), {
      type: "runtime_services_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-services",
        request_id: "request-services",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-state",
      services: [
        { kind: "lsp", name: "rust-analyzer" },
        { kind: "linter", name: "clippy-driver" },
      ],
    })

    expect(state.runtimeServices).toEqual([
      { kind: "lsp", name: "rust-analyzer" },
      { kind: "linter", name: "clippy-driver" },
    ])
    expect(state.commandAcks["request-services"]?.responseType).toBe("runtime_services_listed")
  })

  test("does not infer an active session without a driver client", () => {
    const state = reduce(createInitialState(), {
      type: "sessions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "sessions",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      sessions: [
        { title: "Fixture",
          session_id: "session",
          workspace_name: "Rottweiler",
          model: "model",
          driver_client_id: null,
          shell_active: false,
        },
      ],
    })
    expect(state.model).toBeNull()
  })

  test("projects session title updates into the matching sessions-picker row", () => {
    const listed = reduce(createInitialState(), {
      type: "sessions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "sessions",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      sessions: [
        {
          session_id: "session-state",
          title: "Old title",
          workspace_name: "Rottweiler",
          model: "fast",
          driver_client_id: null,
          shell_active: false,
        },
        {
          session_id: "other-session",
          title: "Keep me",
          workspace_name: "Other",
          model: "fast",
          driver_client_id: null,
          shell_active: false,
        },
      ],
    })
    const renamed = reduce(listed, {
      type: "session_title_updated",
      meta: meta("0"),
      title: "Auth refactor",
    })

    expect(renamed.sessions.map((session) => [session.sessionId, session.title])).toEqual([
      ["session-state", "Auth refactor"],
      ["other-session", "Keep me"],
    ])
  })

  test("projects plugin status and bounded UI notifications as known durable events", () => {
    let state = reduce(createInitialState(), {
      type: "plugin_status_changed",
      meta: meta("1"),
      plugin_id: "formatter",
      status: "watching",
    })
    state = reduce(state, {
      type: "ui_notification",
      meta: meta("2"),
      plugin_id: "formatter",
      title: "Format complete",
      message: "src/main.rs",
    })
    state = reduce(state, {
      type: "plugin_message_injected",
      meta: meta("3"),
      plugin_id: "formatter",
      content: "/help remains plain text",
      queued: true,
    })

    expect(state.pluginStatuses).toEqual({ formatter: "watching" })
    expect(state.pluginNotifications).toEqual([
      { pluginId: "formatter", title: "Format complete", message: "src/main.rs" },
    ])
  })

  test("projects live workspace-root generations using only virtual paths", () => {
    const state = reduce(createInitialState(), {
      type: "workspace_roots_changed",
      meta: meta("1"),
      generation: "1",
      effective_from_turn: "4",
      roots: [
        { index: 0, path: "@root/0", machine_local: false },
        { index: 1, path: "@root/1", machine_local: false },
      ],
    })
    expect(state.workspaceRoots).toEqual({
      generation: "1",
      effectiveFromTurn: "4",
      roots: ["@root/0", "@root/1"],
    })
  })
})
