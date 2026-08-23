import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import {
  DEFAULT_THEMES,
  THEME_ROLE_KEYS,
  daylightTheme,
  isThemeJson,
  kennelTheme,
  loadCustomThemes,
  resolveThemeJson,
  systemThemeFromPalette,
  systemThemeFor,
  themeCatalogFor,
  themeByName,
  themeCatalog,
} from "../src/theme"

let root: string | undefined

afterEach(async () => {
  if (root !== undefined) await rm(root, { recursive: true, force: true })
  root = undefined
  const empty = await mkdtemp(join(tmpdir(), "rottweiler-empty-themes-"))
  await loadCustomThemes(empty)
  await rm(empty, { recursive: true, force: true })
})

describe("themes", () => {
  test("resolves system colors from the renderer-reported terminal mode", () => {
    expect(systemThemeFor("light").background).toBe(daylightTheme.background)
    expect(systemThemeFor("dark").background).toBe(kennelTheme.background)
    expect(systemThemeFor(null).background).toBe(kennelTheme.background)
  })

  test("derives the System theme from the terminal's actual ANSI palette", () => {
    const system = systemThemeFromPalette({
      palette: [
        "#101112", "#AA0000", "#00AA00", "#AAAA00", "#0000AA", "#AA00AA", "#00AAAA", "#CCCCCC",
        "#555555", "#FF0000", "#00FF00", "#FFFF00", "#0000FF", "#FF00FF", "#00FFFF", "#FFFFFF",
      ],
      defaultForeground: "#E1E2E3",
      defaultBackground: "#101112",
    })
    expect(system.name).toBe("system")
    expect(system.mode).toBe("dark")
    expect(system.text).toBe("#E1E2E3")
    expect(system.primary).toBe("#00AAAA")
    expect(system.error).toBe("#AA0000")
    expect(system.background).toBe("#00000000")
  })

  test("ships and resolves the complete OpenCode cf75036 catalog in both modes", () => {
    expect(Object.keys(DEFAULT_THEMES)).toHaveLength(33)
    for (const [name, source] of Object.entries(DEFAULT_THEMES)) {
      for (const mode of ["dark", "light"] as const) {
        const theme = resolveThemeJson(source, mode, name)
        expect(theme.name).toBe(name)
        expect(theme.mode).toBe(mode)
        for (const role of THEME_ROLE_KEYS) expect(theme[role]).toMatch(/^#[0-9A-F]{6}(?:[0-9A-F]{2})?$/)
      }
    }
    expect(themeCatalogFor("light").map((theme) => theme.name)).toEqual(
      ["system", ...Object.keys(DEFAULT_THEMES)],
    )
    expect(themeCatalog).toHaveLength(34)
  })

  test("resolves variants, role references, and optional role defaults", () => {
    const dark = resolveThemeJson(DEFAULT_THEMES.opencode!, "dark", "opencode")
    const light = resolveThemeJson(DEFAULT_THEMES.opencode!, "light", "opencode")
    expect(dark.background).not.toBe(light.background)
    expect(dark.backgroundMenu).toBe(dark.backgroundElement)
    expect(dark.selectedListItemText).toBe(dark.background)
  })

  test("rejects incomplete, unknown-role, missing-reference, and circular themes", () => {
    const valid = structuredClone(DEFAULT_THEMES.opencode!)
    expect(isThemeJson(valid)).toBeTrue()
    const incomplete = structuredClone(valid) as { theme: Record<string, unknown> }
    delete incomplete.theme.primary
    expect(isThemeJson(incomplete)).toBeFalse()
    const unknown = structuredClone(valid) as { theme: Record<string, unknown> }
    unknown.theme.internalRustColor = "#ffffff"
    expect(isThemeJson(unknown)).toBeFalse()
    const missing = structuredClone(valid) as { theme: Record<string, unknown> }
    missing.theme.primary = "does-not-exist"
    expect(isThemeJson(missing)).toBeFalse()
    const circular = structuredClone(valid) as {
      defs?: Record<string, string>
      theme: Record<string, unknown>
    }
    circular.defs = { one: "two", two: "one" }
    circular.theme.primary = "one"
    expect(isThemeJson(circular)).toBeFalse()
  })

  test("loads bounded data-only custom themes and skips malformed files", async () => {
    root = await mkdtemp(join(tmpdir(), "rottweiler-themes-"))
    await mkdir(root, { recursive: true })
    const custom = structuredClone(DEFAULT_THEMES.opencode!) as {
      theme: Record<string, unknown>
    }
    custom.theme.accent = "#123ABC"
    await writeFile(join(root, "my-theme.json"), JSON.stringify(custom))
    await writeFile(join(root, "invalid.json"), JSON.stringify({ theme: { primary: "#fff" } }))
    await writeFile(join(root, "oversized.json"), " ".repeat(129 * 1024))

    await loadCustomThemes(root)

    expect(themeByName("my-theme")?.accent).toBe("#123ABC")
    expect(themeByName("invalid")).toBeUndefined()
    expect(themeCatalog.map((theme) => theme.name)).toContain("my-theme")
  })
})
