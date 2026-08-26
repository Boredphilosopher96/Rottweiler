import { describe, expect, test } from "bun:test"

import {
  createSettingsBrowserModel,
  type SettingsCatalog,
} from "../src/settings-browser"
import type { RottweilerState } from "../src/state/model"

type Setting = RottweilerState["settings"][number]

const setting = (value: Setting): Setting => value

const settings: readonly Setting[] = [
  setting({ key: "models.thinking.fast", label: "Thinking · fast", value: "medium", choices: ["off", "low", "medium", "high"], provenance: "user", appliesImmediately: false }),
  setting({ key: "permissions.default", label: "Default permission", value: "ask", choices: ["ask", "allow", "deny"], provenance: "project", appliesImmediately: true }),
  setting({ key: "compaction.auto", label: "Automatic compaction", value: "true", choices: ["true", "false"], provenance: "built-in", appliesImmediately: false }),
  setting({ key: "budget.session_token_cap", label: "Session token cap", value: "250000", choices: [], provenance: "user", appliesImmediately: false }),
  setting({ key: "mcp.servers.docs.enabled", label: "MCP · docs", value: "true", choices: ["true", "false"], provenance: "user MCP configuration", appliesImmediately: false }),
  setting({ key: "ui.theme", label: "Theme", value: "opencode", choices: [], provenance: "user", appliesImmediately: false }),
  setting({ key: "ui.keybindings.preset", label: "Keybinding preset", value: "vim", choices: ["standard", "vim"], provenance: "user keybindings", appliesImmediately: false }),
  setting({ key: "project.models.default", label: "Project default model", value: "fast", choices: ["fast"], provenance: "private project preference", appliesImmediately: false }),
  setting({ key: "future.setting", label: "Future setting", value: "on", choices: ["on", "off"], provenance: "future", appliesImmediately: false }),
  setting({ key: "future.read_only", label: "Future read-only setting", value: "fixed", choices: [], provenance: "future", appliesImmediately: false }),
]

describe("settings browser model", () => {
  test("projects authoritative settings into ordered sections, candidates, and handoffs", () => {
    const model = createSettingsBrowserModel({
      catalog: { kind: "ready", settings },
      query: "",
      selectedId: "models.thinking.fast",
    })

    expect(model.rows.filter((row) => row.kind === "section").map((row) => row.label)).toEqual([
      "Model & routing",
      "Permissions",
      "Context & compaction",
      "Budget & guardrails",
      "MCP servers",
      "Appearance",
      "Keybindings",
      "Other",
    ])
    expect(model.selectedId).toBe("models.thinking.fast")
    expect(model.rows.find((row) => row.id === "models.thinking.fast")).toMatchObject({
      kind: "item",
      label: "Thinking · fast",
      action: { kind: "choose", key: "models.thinking.fast" },
      detail: {
        title: "Thinking · fast",
        meta: "models.thinking.fast",
        description: "current    medium\nchoices    off · low · medium · high\nsource     user\napplies    next session",
      },
    })
    expect(model.rows.find((row) => row.id === "ui.theme")).toMatchObject({ action: { kind: "openThemes" } })
    expect(model.rows.find((row) => row.id === "budget.manage")).toMatchObject({ action: { kind: "openBudgets" } })
    expect(model.rows.find((row) => row.id === "project.models.default")).toMatchObject({ action: { kind: "inspect" } })
    expect(model.rows.some((row) => row.id === "future.setting")).toBeTrue()
    expect(model.rows.find((row) => row.id === "future.read_only")).toMatchObject({
      kind: "item",
      label: "Future read-only setting",
      action: { kind: "inspect" },
    })
    expect(model.rows.find((row) => row.id === "compaction.auto")?.label).toBe("Automatic compaction")
    expect(model.rows.find((row) => row.id === "compaction.auto")?.label).not.toContain("true")
    expect(model.rows.find((row) => row.id === "project.models.default")?.label).toBe("Project default model")
    expect(model.rows.find((row) => row.id === "ui.theme")?.label).toBe("Theme")
    expect(JSON.stringify(model)).not.toMatch(/\bsave\b|\breset\b|\bdiscard\b|changed keys|config\.toml|\bdiff\b/i)
  })

  test("filters fuzzily, retains a visible selection, and distinguishes request states", () => {
    const filtered = createSettingsBrowserModel({
      catalog: { kind: "ready", settings },
      query: "compaction.auto",
      selectedId: "models.thinking.fast",
    })
    expect(filtered.rows.filter((row) => row.kind === "item").map((row) => row.id)).toEqual([
      "compaction.auto",
    ])
    expect(filtered.selectedId).toBe("compaction.auto")

    const loading = createSettingsBrowserModel({ catalog: { kind: "loading" }, query: "", selectedId: null })
    expect(loading).toMatchObject({ rows: [], selectedId: null, emptyCopy: "Loading settings" })

    const failed: SettingsCatalog = { kind: "error", message: "settings unavailable", stale: [] }
    expect(createSettingsBrowserModel({ catalog: failed, query: "", selectedId: null })).toMatchObject({
      rows: [],
      emptyCopy: "settings unavailable",
      status: "Ctrl-R retry",
      notice: { message: "settings unavailable", tone: "error" },
    })
  })
})
