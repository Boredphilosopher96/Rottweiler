import { describe, expect, test } from "bun:test"

import { createInitialState } from "../src/state"
import {
  boundedUiText,
  contextPanelHasContent,
  modeDisplayName,
  modePickerPresentation,
  modelAliasDescription,
  nextModeId,
  permissionPatternLabel,
  providerName,
  providerStatusDetail,
  queuedMessageLabel,
  timelineTurnLabel,
} from "../src/ui-presentation"

describe("pure UI presentation policy", () => {
  test("sanitizes and bounds labels before they reach retained controls", () => {
    expect(boundedUiText("  hello\u0000\n  world  ", 64)).toBe("hello world")
    expect(queuedMessageLabel("\nsecond line")).toBe("(empty message)")
    expect(timelineTurnLabel("")).toBe("(attachment-only message)")
  })

  test("uses product names and actionable provider failure copy", () => {
    expect(providerName("openai_codex")).toBe("OpenAI · ChatGPT")
    expect(providerName("github_copilot")).toBe("GitHub Copilot")
    expect(providerName("custom_gateway")).toBe("Custom Gateway")
    expect(providerStatusDetail({
      name: "github_copilot",
      authKind: "device_flow",
      nextAction: "select_models",
      configured: true,
      authenticated: true,
      reachable: false,
      modelCount: 0,
      status: "network timed out",
    })).toBe("Couldn't reach the model catalog · retry")
  })

  test("shows the context sidebar only for presentable content", () => {
    const empty = createInitialState()
    expect(contextPanelHasContent(empty)).toBeFalse()
    expect(contextPanelHasContent({
      ...empty,
      runtimeServices: [{ kind: "lsp", name: "" }],
    })).toBeFalse()
    expect(contextPanelHasContent({
      ...empty,
      todos: [{ id: "one", content: "Inspect", status: "pending" }],
    })).toBeTrue()
  })

  test("presents and cycles custom modes in the engine-provided order", () => {
    const modes = [
      { id: "execute", description: "Make changes", current: true },
      { id: "audit", description: "Inspect controls", current: false },
      { id: "plan_only", description: "Plan", current: false },
    ]
    expect(modeDisplayName("plan_only")).toBe("Plan only")
    expect(nextModeId("execute", modes)).toBe("audit")
    expect(nextModeId("audit", modes)).toBe("plan_only")
    expect(nextModeId("missing", modes)).toBe("execute")
  })

  test("builds truthful bounded mode-picker loading, failure, and partial states", () => {
    const state = {
      mode: "audit",
      modes: [
        { id: "execute", description: "Make changes", current: false },
        { id: "audit", description: `Inspect\u0000 ${"evidence ".repeat(40)}`, current: true },
      ],
      modesTruncated: true,
    }
    const loading = modePickerPresentation(state, undefined, true)
    expect(loading.title).toBe("Modes · refreshing")
    expect(loading.items[1]?.label).toBe("● Audit")
    expect(loading.items[1]?.description.length).toBeLessThanOrEqual(160)

    const failed = modePickerPresentation(state, "unsafe\u0000 failure", false)
    expect(failed.title).toBe("Modes · load failed")
    expect(failed.items[0]).toMatchObject({
      id: "modes.retry",
      description: "unsafe failure",
      value: { kind: "retry" },
    })

    expect(modePickerPresentation(state, undefined, false).title).toBe("Modes · partial catalog")
  })

  test("summarizes model routes and permission patterns consistently", () => {
    expect(modelAliasDescription(
      { alias: "fast", candidates: ["openai/gpt-5", "copilot/gpt-5"], current: false },
      [
        { id: "openai/gpt-5", displayName: "gpt-5", provider: "openai", aliases: ["fast"], current: false, status: null, vision: true, thinking: true, toolCalling: true, available: false },
        { id: "copilot/gpt-5", displayName: "gpt-5", provider: "copilot", aliases: ["fast"], current: false, status: null, vision: true, thinking: true, toolCalling: true, available: false },
      ],
    )).toContain("no available route")
    expect(permissionPatternLabel("bash(*)")).toBe("bash · any arguments")
    expect(permissionPatternLabel("read(src/**)")).toBe("read · arguments matching src/**")
  })
})
