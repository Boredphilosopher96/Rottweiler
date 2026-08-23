import { describe, expect, test } from "bun:test"

import {
  commandSourceLabel,
  isTuiHandledSlashCommand,
  isU64,
  TUI_SLASH_COMMANDS,
  mergeSlashCommandChoices,
  parseSessionAction,
} from "../src/session-commands"

describe("session command policy", () => {
  test("keeps the TUI catalog unique and merges engine descriptors in stable order", () => {
    const names = TUI_SLASH_COMMANDS.map((command) => command.name)
    expect(new Set(names).size).toBe(names.length)
    expect(TUI_SLASH_COMMANDS.find((command) => command.name === "models")?.usage).toBe("/models")
    expect(TUI_SLASH_COMMANDS.find((command) => command.name === "mode")).toBeUndefined()
    expect(isTuiHandledSlashCommand("models")).toBeTrue()
    expect(isTuiHandledSlashCommand("review")).toBeTrue()
    expect(isTuiHandledSlashCommand("status")).toBeFalse()
    expect(isTuiHandledSlashCommand("deploy")).toBeFalse()

    const merged = mergeSlashCommandChoices([
      { name: "models", description: "Live model catalog", usage: "/models", source: "builtin" },
      { name: "deploy", description: "Deploy", usage: "/deploy", source: "plugin" },
    ])
    expect(merged.find((command) => command.name === "models")?.description).toBe("Live model catalog")
    expect(merged.at(-1)?.name).toBe("deploy")
  })

  test("parses TUI-owned actions without intercepting engine command arguments", () => {
    expect(parseSessionAction("  /exit  ")).toEqual({ type: "exit" })
    expect(parseSessionAction("/new")).toEqual({ type: "new" })
    expect(parseSessionAction("/new now")).toEqual({ type: "invalid", message: "usage: /new" })
    expect(parseSessionAction("/rewind")).toEqual({ type: "rewindTimeline" })
    expect(parseSessionAction("/models")).toEqual({ type: "models" })
    expect(parseSessionAction("/providers")).toEqual({ type: "providers" })
    expect(parseSessionAction("/agents")).toEqual({ type: "agents" })
    expect(parseSessionAction("/theme")).toEqual({ type: "theme" })
    expect(parseSessionAction("/settings")).toEqual({ type: "settings" })
    expect(parseSessionAction("/permissions")).toEqual({ type: "permissions" })
    expect(parseSessionAction("/permissions list")).toBeNull()
    expect(parseSessionAction("/mcp add")).toBeNull()
    expect(parseSessionAction("/status")).toBeNull()
  })

  test("accepts only canonical decimal u64 fork targets", () => {
    expect(parseSessionAction("/fork")).toEqual({ type: "fork", atTurn: null })
    expect(parseSessionAction("/fork 18446744073709551615")).toEqual({
      type: "fork",
      atTurn: "18446744073709551615",
    })
    for (const value of ["01", "-1", "+1", "1.0", "18446744073709551616"]) {
      expect(isU64(value)).toBeFalse()
      expect(parseSessionAction(`/fork ${value}`)).toEqual({
        type: "invalid",
        message: "usage: /fork [turn] where turn is a decimal u64",
      })
    }
  })

  test("labels command provenance for picker presentation", () => {
    expect(commandSourceLabel(undefined)).toBe("Built-in")
    expect(commandSourceLabel("project")).toBe("Project")
    expect(commandSourceLabel("plugin")).toBe("Plugin")
    expect(commandSourceLabel("mcp")).toBe("MCP")
  })
})
