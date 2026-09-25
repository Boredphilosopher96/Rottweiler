import { describe, expect, test } from "bun:test"

import {
  BUILTIN_COMMANDS,
  catalogCommand,
  commandSourceLabel,
  resolveSlashInput,
} from "../src/session-commands"

describe("slash command resolution", () => {
  test("projects one engine-owned catalog without duplicate names or aliases", () => {
    const names = BUILTIN_COMMANDS.flatMap((command) => [command.name, ...command.aliases])
    expect(new Set(names).size).toBe(names.length)
    for (const removed of ["goto", "status", "interrupt", "plan", "fork", "workflow-status", "deep-init", "mcp.prompt"]) {
      expect(catalogCommand(removed)).toBeUndefined()
    }
    expect(catalogCommand("providers")?.name).toBe("model")
    expect(catalogCommand("sessions")?.name).toBe("resume")
  })

  test("opens screens for bare catalog entries and rewrites aliases before the engine", () => {
    expect(resolveSlashInput("  /exit  ")).toEqual({ type: "screen", name: "exit" })
    expect(resolveSlashInput("/models")).toEqual({ type: "screen", name: "model" })
    expect(resolveSlashInput("/trust")).toEqual({ type: "screen", name: "permissions" })
    expect(resolveSlashInput("/rewind")).toEqual({ type: "screen", name: "rewind" })
    expect(resolveSlashInput("/new now")).toEqual({ type: "invalid", message: "usage: /new" })
    expect(resolveSlashInput("/rewind 3")).toEqual({ type: "engine", content: "/rewind 3" })
    expect(resolveSlashInput("/mode plan")).toEqual({ type: "engine", content: "/mode plan" })
    expect(resolveSlashInput("/add-dir ../docs")).toEqual({ type: "engine", content: "/dirs ../docs" })
    expect(resolveSlashInput("/compact")).toEqual({ type: "engine", content: "/compact" })
    expect(resolveSlashInput("/compact keep the API notes")).toEqual({
      type: "engine",
      content: "/compact keep the API notes",
    })
    expect(resolveSlashInput("/deploy prod")).toBeNull()
    expect(resolveSlashInput("hello /model")).toBeNull()
  })

  test("labels extension provenance", () => {
    expect(commandSourceLabel({ name: "review", source: "skill" })).toBe("skill")
    expect(commandSourceLabel({ name: "deploy", source: "project" })).toBe("project command")
    expect(commandSourceLabel({ name: "mcp.github.triage", source: "mcp" })).toBe("mcp · github")
    expect(commandSourceLabel({ name: "workflow", source: "workflow" })).toBe("workflow")
  })
})
