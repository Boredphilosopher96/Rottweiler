import { describe, expect, test } from "bun:test"

import {
  createCommandList,
  createCommandPaletteModel,
  rankCommands,
  rememberCommand,
  type CommandEntry,
} from "../src/command-palette"

function entry(
  name: string,
  title: string,
  section: CommandEntry<string>["section"],
  options: Partial<CommandEntry<string>> = {},
): CommandEntry<string> {
  return {
    id: `cmd.${name}`, name, aliases: [], title, section, description: `${title} description`,
    argumentHint: "", keycap: null, sourceLabel: null, unavailableReason: null, action: name, ...options,
  }
}

const entries: readonly CommandEntry<string>[] = [
  entry("new", "New session", "Conversation"),
  entry("rewind", "Rewind", "Conversation", { unavailableReason: "Stop the current turn or wait for it to finish." }),
  entry("compact", "Compact", "Conversation", { description: "Summarize older context to free space" }),
  entry("model", "Model", "Models & agents", { aliases: ["models", "providers"], description: "Choose a model or connect a provider" }),
  entry("mode", "Mode", "Models & agents"),
  entry("context", "Context", "Context & usage"),
  entry("usage", "Usage", "Context & usage", { aliases: ["cost", "budget"] }),
  entry("dirs", "Directories", "Workspace", { aliases: ["add-dir"] }),
  entry("mcp", "MCP servers", "Workspace"),
  entry("permissions", "Permissions", "Safety", { aliases: ["trust"] }),
  entry("help", "Help", "Settings & help", { aliases: ["keys"] }),
  entry("deploy", "deploy", "Extensions", { id: "ext.deploy", sourceLabel: "project command", description: "Deploy the project" }),
]

const ids = (query: string, recent: readonly string[] = []) =>
  rankCommands(entries, query, recent).map((ranked) => ranked.entry.id)

describe("command ranking", () => {
  test("exact name or alias beats name prefix, title word prefix, fuzzy title, and description", () => {
    expect(ids("providers")[0]).toBe("cmd.model")
    expect(ids("/cost")[0]).toBe("cmd.usage")
    expect(ids("mod")).toEqual(["cmd.model", "cmd.mode"])
    expect(ids("mode")[0]).toBe("cmd.mode")
    expect(ids("servers")[0]).toBe("cmd.mcp")
    expect(ids("summarize")).toEqual(["cmd.compact"])
    expect(ids("dirctr")[0]).toBe("cmd.dirs")
    expect(ids("nothing-matches")).toEqual([])
  })

  test("ties prefer recent use, then section order", () => {
    expect(ids("m")).toEqual(["cmd.model", "cmd.mode", "cmd.mcp"])
    expect(ids("m", ["cmd.mcp"])[0]).toBe("cmd.mcp")
    expect(ids("co").slice(0, 2)).toEqual(["cmd.compact", "cmd.context"])
    expect(ids("co", ["cmd.context"]).slice(0, 2)).toEqual(["cmd.context", "cmd.compact"])
  })

  test("remembers the most recent uses without duplicates", () => {
    expect(rememberCommand(["cmd.a", "cmd.b"], "cmd.b")).toEqual(["cmd.b", "cmd.a"])
    expect(rememberCommand(Array.from({ length: 8 }, (_, index) => `cmd.${index}`), "cmd.new")).toHaveLength(8)
  })
})

describe("command list presentation", () => {
  test("groups an empty query with Recent first and unavailable entries last in their section", () => {
    const list = createCommandList(entries, "", ["cmd.help", "cmd.rewind", "cmd.mcp", "cmd.dirs", "cmd.new"])
    const shape = list.rows.map((row) => row.kind === "section" ? `# ${row.label}` : row.id)
    expect(shape.slice(0, 4)).toEqual(["# Recent", "cmd.help", "cmd.mcp", "cmd.dirs"])
    expect(shape).toContain("# Extensions")
    const conversation = shape.slice(shape.indexOf("# Conversation") + 1, shape.indexOf("# Models & agents"))
    expect(conversation).toEqual(["cmd.new", "cmd.compact", "cmd.rewind"])
    expect(shape.filter((id) => id === "cmd.help")).toHaveLength(1)
    expect(list.selectedId).toBe("cmd.help")
  })

  test("never selects an unavailable entry initially and filters without headers", () => {
    const list = createCommandList(entries, "rew", [])
    expect(list.rows.map((row) => row.id)).toEqual(["cmd.rewind"])
    expect(list.selectedId).toBeNull()
    const retained = createCommandList(entries, "", [], "cmd.rewind")
    expect(retained.selectedId).toBe("cmd.new")
  })

  test("reports counts and catalog notices", () => {
    const model = createCommandPaletteModel({
      entries, query: "mo", recent: [], selectedId: null, catalog: { kind: "ready", truncated: true },
    })
    expect(model.status).toBe(`${model.visible} of ${entries.length} commands`)
    expect(model.notice).toEqual({ kind: "truncated", message: "Extension results are truncated" })
    expect(createCommandPaletteModel({
      entries, query: "", recent: [], selectedId: null, catalog: { kind: "error", message: "catalog unavailable" },
    }).notice).toEqual({ kind: "error", message: "catalog unavailable" })
    expect(createCommandPaletteModel({
      entries, query: "", recent: [], selectedId: null, catalog: { kind: "loading" },
    }).notice).toEqual({ kind: "loading", message: "Loading extension commands…" })
  })
})
