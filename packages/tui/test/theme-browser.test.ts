import { describe, expect, test } from "bun:test"

import { createThemeBrowserModel } from "../src/theme-browser"
import {
  THEME_ROLE_KEYS,
  themeByName,
  themeCatalogFor,
} from "../src/theme"

describe("theme browser model", () => {
  test("preserves catalog order and derives counts, five semantic swatches, and all roles", () => {
    const catalog = themeCatalogFor("dark")
    const model = createThemeBrowserModel({
      themes: catalog,
      query: "",
      selectedName: "opencode",
      currentName: "opencode",
    })

    expect(model.counts).toEqual({ visible: 34, total: 34, custom: 0 })
    expect(model.rows.map((row) => row.name)).toEqual(catalog.map((theme) => theme.name))
    expect(model.selectedId).toBe("theme:opencode")
    expect(model.rows.find((row) => row.name === "opencode")).toMatchObject({
      active: true,
      source: "builtin",
      swatches: [
        { role: "background", color: catalog.find((theme) => theme.name === "opencode")!.background },
        { role: "primary", color: catalog.find((theme) => theme.name === "opencode")!.primary },
        { role: "accent", color: catalog.find((theme) => theme.name === "opencode")!.accent },
        { role: "success", color: catalog.find((theme) => theme.name === "opencode")!.success },
        { role: "error", color: catalog.find((theme) => theme.name === "opencode")!.error },
      ],
    })
    expect(model.detail).toMatchObject({
      kind: "theme",
      name: "opencode",
      mode: "dark",
      roleCount: 52,
    })
    if (model.detail.kind !== "theme") throw new Error("expected selected theme detail")
    expect(Object.keys(model.detail.roles)).toEqual([...THEME_ROLE_KEYS])
    expect(model.status).toBe("34 themes · dark · 0 custom")
    expect(model.footer).toBe("arrows preview · Enter apply · Esc cancel")
  })

  test("filters fuzzily while retaining a stable visible selection", () => {
    const catalog = themeCatalogFor("dark")
    const retained = createThemeBrowserModel({
      themes: catalog,
      query: "tkn",
      selectedName: "tokyonight",
      currentName: "opencode",
    })

    expect(retained.rows.map((row) => row.name)).toEqual(["tokyonight"])
    expect(retained.rows[0]?.matchSpans.length).toBeGreaterThan(0)
    expect(retained.selectedId).toBe("theme:tokyonight")
    expect(retained.status).toBe("1 of 34 themes · dark · 0 custom")

    const fallback = createThemeBrowserModel({
      themes: catalog,
      query: "nord",
      selectedName: "tokyonight",
      currentName: "opencode",
    })
    expect(fallback.selectedId).toBe("theme:nord")
  })

  test("resolves dark, light, and System rows from the supplied catalog", () => {
    for (const mode of ["dark", "light"] as const) {
      const catalog = themeCatalogFor(mode)
      const model = createThemeBrowserModel({
        themes: catalog,
        query: "",
        selectedName: "system",
        currentName: "system",
      })
      const system = catalog.find((theme) => theme.name === "system")!

      expect(model.detail).toMatchObject({ kind: "theme", name: "system", mode })
      expect(model.rows[0]).toMatchObject({
        name: "system",
        mode,
        source: "system",
        swatches: [
          { role: "background", color: system.background },
          { role: "primary", color: system.primary },
          { role: "accent", color: system.accent },
          { role: "success", color: system.success },
          { role: "error", color: system.error },
        ],
      })
    }
  })

  test("derives custom-theme truthfulness and an explicit empty result", () => {
    const system = themeCatalogFor("dark")[0]!
    const builtin = themeByName("opencode", "dark")!
    const custom = { ...builtin, name: "fixture-custom" }
    const populated = createThemeBrowserModel({
      themes: [system, builtin, custom],
      query: "",
      selectedName: "fixture-custom",
      currentName: "opencode",
    })

    expect(populated.counts).toEqual({ visible: 3, total: 3, custom: 1 })
    expect(populated.rows.at(-1)).toMatchObject({
      name: "fixture-custom",
      source: "custom",
    })
    expect(populated.customThemeDirectory).toBe("~/.rottweiler/themes/")

    const empty = createThemeBrowserModel({
      themes: [system, builtin, custom],
      query: "no-such-theme",
      selectedName: "fixture-custom",
      currentName: "opencode",
    })
    expect(empty.rows).toEqual([])
    expect(empty.selectedId).toBeNull()
    expect(empty.detail).toEqual({ kind: "empty", message: "No matching themes" })
    expect(empty.status).toBe("0 of 3 themes · dark · 1 custom")
  })
})
