import { describe, expect, test } from "bun:test"

import {
  createCommandPaletteModel,
  fuzzyMatch,
  retainCommandPaletteSelection,
  type CommandPaletteEntry,
} from "../src/command-palette"

const sections = ["Conversation", "Workspace", "Commands"] as const

const entries: readonly CommandPaletteEntry<string>[] = [
  {
    id: "session.new",
    title: "New session",
    description: "Start a clean conversation",
    section: "Conversation",
    source: "builtin",
    action: "new",
  },
  {
    id: "workspace.roots",
    title: "Workspace roots",
    description: "See every live workspace root",
    section: "Workspace",
    source: "builtin",
    action: "roots",
  },
  {
    id: "slash.deploy",
    title: "/deploy",
    description: "Project · Deploy the project",
    section: "Commands",
    source: "extension",
    action: "deploy",
  },
]

describe("command palette presentation model", () => {
  test("groups an empty query and derives source and result counts from entries", () => {
    const model = createCommandPaletteModel({
      entries,
      sections,
      query: "",
      selectedId: null,
      catalog: { kind: "ready", truncated: false },
    })

    expect(model.rows.map((row) => row.kind === "section" ? row.label : row.id)).toEqual([
      "Conversation",
      "session.new",
      "Workspace",
      "workspace.roots",
      "Commands",
      "slash.deploy",
    ])
    expect(model.counts).toEqual({ visible: 3, total: 3, builtIn: 2, extension: 1 })
    expect(model.status).toBe("3 commands · 2 built-in · 1 extension")
  })

  test("ranks fuzzy matches while retaining complete match spans", () => {
    const model = createCommandPaletteModel({
      entries,
      sections,
      query: "work root",
      selectedId: null,
      catalog: { kind: "ready", truncated: false },
    })

    expect(model.rows).toHaveLength(1)
    expect(model.rows[0]).toMatchObject({
      kind: "item",
      id: "workspace.roots",
      titleMatches: [[0, 4], [10, 14]],
    })
    expect(model.status).toBe("1 of 3 commands · 2 built-in · 1 extension")
    expect(fuzzyMatch("ns", "New session")).toMatchObject({ positions: [0, 4] })
  })

  test("shows detail only for the selected action", () => {
    const selected = createCommandPaletteModel({
      entries,
      sections,
      query: "",
      selectedId: "workspace.roots",
      catalog: { kind: "ready", truncated: false },
    })
    const missing = createCommandPaletteModel({
      entries,
      sections,
      query: "missing",
      selectedId: "workspace.roots",
      catalog: { kind: "ready", truncated: false },
    })

    expect(selected.detail).toEqual({
      kind: "command",
      id: "workspace.roots",
      title: "Workspace roots",
      description: "See every live workspace root",
      section: "Workspace",
      source: "builtin",
    })
    expect(missing.detail).toEqual({ kind: "empty", message: "No matching commands" })
  })

  test("keeps loading, error, empty, and truncated catalog states distinct", () => {
    const loading = createCommandPaletteModel({
      entries: entries.slice(0, 2), sections, query: "", selectedId: null,
      catalog: { kind: "loading" },
    })
    const failed = createCommandPaletteModel({
      entries: entries.slice(0, 2), sections, query: "", selectedId: null,
      catalog: { kind: "error", message: "catalog unavailable", retryable: true },
    })
    const empty = createCommandPaletteModel({
      entries: [], sections, query: "", selectedId: null,
      catalog: { kind: "ready", truncated: false },
    })
    const truncated = createCommandPaletteModel({
      entries, sections, query: "", selectedId: null,
      catalog: { kind: "ready", truncated: true },
    })

    expect(loading.notice).toEqual({ kind: "loading", message: "Loading extension commands…" })
    expect(failed.notice).toEqual({
      kind: "error", message: "catalog unavailable", retryable: true,
    })
    expect(empty.detail).toEqual({ kind: "empty", message: "No commands available" })
    expect(truncated.notice).toEqual({
      kind: "truncated", message: "Extension results are truncated",
    })
  })

  test("retains a stable selected ID and otherwise chooses the first visible action", () => {
    expect(retainCommandPaletteSelection(entries, "workspace.roots")).toBe("workspace.roots")
    expect(retainCommandPaletteSelection(entries.slice(1), "session.new")).toBe("workspace.roots")
    expect(retainCommandPaletteSelection([], "session.new")).toBeNull()
  })
})
