import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import {
  daylightTheme,
  kennelTheme,
  loadCustomThemes,
  systemThemeFor,
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

  test("loads bounded data-only custom themes and skips malformed files", async () => {
    root = await mkdtemp(join(tmpdir(), "rottweiler-themes-"))
    await mkdir(root, { recursive: true })
    const custom = { ...kennelTheme, name: "my-theme", accent: "#123ABC" }
    await writeFile(join(root, "valid.json"), JSON.stringify(custom))
    await writeFile(join(root, "invalid.json"), JSON.stringify({ name: "../../bad" }))
    await writeFile(join(root, "oversized.json"), " ".repeat(33 * 1024))

    await loadCustomThemes(root)

    expect(themeByName("my-theme")?.accent).toBe("#123ABC")
    expect(themeByName("../../bad")).toBeUndefined()
    expect(themeCatalog.map((theme) => theme.name)).toContain("my-theme")
  })
})
