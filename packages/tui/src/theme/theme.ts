import { SyntaxStyle } from "@opentui/core"
import { lstat, readFile, readdir } from "node:fs/promises"
import { basename, join } from "node:path"

import aura from "./assets/aura.json"
import ayu from "./assets/ayu.json"
import carbonfox from "./assets/carbonfox.json"
import catppuccinFrappe from "./assets/catppuccin-frappe.json"
import catppuccinMacchiato from "./assets/catppuccin-macchiato.json"
import catppuccin from "./assets/catppuccin.json"
import cobalt2 from "./assets/cobalt2.json"
import cursor from "./assets/cursor.json"
import dracula from "./assets/dracula.json"
import everforest from "./assets/everforest.json"
import flexoki from "./assets/flexoki.json"
import github from "./assets/github.json"
import gruvbox from "./assets/gruvbox.json"
import kanagawa from "./assets/kanagawa.json"
import lucentOrng from "./assets/lucent-orng.json"
import material from "./assets/material.json"
import matrix from "./assets/matrix.json"
import mercury from "./assets/mercury.json"
import monokai from "./assets/monokai.json"
import nightowl from "./assets/nightowl.json"
import nord from "./assets/nord.json"
import oneDark from "./assets/one-dark.json"
import opencode from "./assets/opencode.json"
import orng from "./assets/orng.json"
import osakaJade from "./assets/osaka-jade.json"
import palenight from "./assets/palenight.json"
import rosepine from "./assets/rosepine.json"
import solarized from "./assets/solarized.json"
import synthwave84 from "./assets/synthwave84.json"
import tokyonight from "./assets/tokyonight.json"
import vercel from "./assets/vercel.json"
import vesper from "./assets/vesper.json"
import zenburn from "./assets/zenburn.json"

export type ThemeMode = "dark" | "light"
export interface TerminalThemeColors {
  readonly palette: readonly (string | null)[]
  readonly defaultForeground: string | null
  readonly defaultBackground: string | null
}
type HexColor = `#${string}`
type ThemeVariant = Readonly<{ dark: ThemeColorValue; light: ThemeColorValue }>
export type ThemeColorValue = HexColor | "transparent" | "none" | string | ThemeVariant

export const THEME_ROLE_KEYS = [
  "primary", "secondary", "accent", "error", "warning", "success", "info",
  "text", "textMuted", "selectedListItemText", "background", "backgroundPanel",
  "backgroundElement", "backgroundMenu", "border", "borderActive", "borderSubtle",
  "diffAdded", "diffRemoved", "diffContext", "diffHunkHeader", "diffHighlightAdded",
  "diffHighlightRemoved", "diffAddedBg", "diffRemovedBg", "diffContextBg",
  "diffLineNumber", "diffAddedLineNumberBg", "diffRemovedLineNumberBg",
  "markdownText", "markdownHeading", "markdownLink", "markdownLinkText",
  "markdownCode", "markdownBlockQuote", "markdownEmph", "markdownStrong",
  "markdownHorizontalRule", "markdownListItem", "markdownListEnumeration",
  "markdownImage", "markdownImageText", "markdownCodeBlock", "syntaxComment",
  "syntaxKeyword", "syntaxFunction", "syntaxVariable", "syntaxString", "syntaxNumber",
  "syntaxType", "syntaxOperator", "syntaxPunctuation",
] as const

export type ThemeRole = typeof THEME_ROLE_KEYS[number]
type RequiredThemeRole = Exclude<ThemeRole, "selectedListItemText" | "backgroundMenu">
export type ResolvedThemeRoles = Readonly<Record<ThemeRole, string>>

export interface ThemeJson {
  readonly $schema?: string
  readonly defs?: Readonly<Record<string, ThemeColorValue>>
  readonly theme: Readonly<Record<RequiredThemeRole, ThemeColorValue>> & Readonly<{
    selectedListItemText?: ThemeColorValue
    backgroundMenu?: ThemeColorValue
    thinkingOpacity?: number
  }>
}

/** Canonical resolved theme roles consumed directly by every TUI component. */
export type RottweilerTheme = ResolvedThemeRoles & Readonly<{
  name: string
  mode: ThemeMode
  thinkingOpacity: number
}>

const asTheme = (value: unknown): ThemeJson => value as ThemeJson

/** Theme assets synchronized from OpenCode dev cf75036. */
export const DEFAULT_THEMES: Readonly<Record<string, ThemeJson>> = {
  aura: asTheme(aura),
  ayu: asTheme(ayu),
  carbonfox: asTheme(carbonfox),
  catppuccin: asTheme(catppuccin),
  "catppuccin-frappe": asTheme(catppuccinFrappe),
  "catppuccin-macchiato": asTheme(catppuccinMacchiato),
  cobalt2: asTheme(cobalt2),
  cursor: asTheme(cursor),
  dracula: asTheme(dracula),
  everforest: asTheme(everforest),
  flexoki: asTheme(flexoki),
  github: asTheme(github),
  gruvbox: asTheme(gruvbox),
  kanagawa: asTheme(kanagawa),
  "lucent-orng": asTheme(lucentOrng),
  material: asTheme(material),
  matrix: asTheme(matrix),
  mercury: asTheme(mercury),
  monokai: asTheme(monokai),
  nightowl: asTheme(nightowl),
  nord: asTheme(nord),
  "one-dark": asTheme(oneDark),
  opencode: asTheme(opencode),
  orng: asTheme(orng),
  "osaka-jade": asTheme(osakaJade),
  palenight: asTheme(palenight),
  rosepine: asTheme(rosepine),
  solarized: asTheme(solarized),
  synthwave84: asTheme(synthwave84),
  tokyonight: asTheme(tokyonight),
  vercel: asTheme(vercel),
  vesper: asTheme(vesper),
  zenburn: asTheme(zenburn),
}

const REQUIRED_THEME_ROLE_KEYS = THEME_ROLE_KEYS.filter(
  (key): key is RequiredThemeRole => key !== "selectedListItemText" && key !== "backgroundMenu",
)
const THEME_NAME = /^[a-z0-9][a-z0-9._-]{0,63}$/
const REFERENCE_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/
const HEX = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$/
const MAX_DEFINITIONS = 256
const MAX_RESOLUTION_DEPTH = 128

export function resolveThemeJson(source: ThemeJson, mode: ThemeMode, name: string): RottweilerTheme {
  const defs = source.defs ?? {}
  const resolve = (value: ThemeColorValue, chain: readonly string[] = []): string => {
    if (typeof value === "object" && value !== null) return resolve(value[mode], chain)
    if (value === "transparent" || value === "none") return "#00000000"
    if (HEX.test(value)) return normalizeHex(value)
    if (!REFERENCE_NAME.test(value)) throw new Error(`Invalid color reference: ${value}`)
    if (chain.length >= MAX_RESOLUTION_DEPTH) throw new Error("Theme color reference depth exceeded")
    if (chain.includes(value)) throw new Error(`Circular color reference: ${[...chain, value].join(" -> ")}`)
    const next = defs[value] ?? source.theme[value as RequiredThemeRole]
    if (next === undefined) throw new Error(`Color reference not found: ${value}`)
    return resolve(next, [...chain, value])
  }

  const roles = Object.fromEntries(REQUIRED_THEME_ROLE_KEYS.map((key) => [key, resolve(source.theme[key])])) as
    Record<ThemeRole, string>
  roles.selectedListItemText = source.theme.selectedListItemText === undefined
    ? roles.background
    : resolve(source.theme.selectedListItemText)
  roles.backgroundMenu = source.theme.backgroundMenu === undefined
    ? roles.backgroundElement
    : resolve(source.theme.backgroundMenu)
  const opacity = source.theme.thinkingOpacity ?? 0.6
  if (!Number.isFinite(opacity) || opacity < 0 || opacity > 1) {
    throw new Error("thinkingOpacity must be between 0 and 1")
  }
  return {
    ...roles,
    name,
    mode,
    thinkingOpacity: opacity,
  }
}

function normalizeHex(value: string): string {
  if (value.length !== 4) return value.toUpperCase()
  return `#${value[1]}${value[1]}${value[2]}${value[2]}${value[3]}${value[3]}`.toUpperCase()
}

export function isThemeJson(value: unknown): value is ThemeJson {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false
  const record = value as Record<string, unknown>
  if (record.defs !== undefined) {
    if (typeof record.defs !== "object" || record.defs === null || Array.isArray(record.defs)) return false
    const definitions = Object.entries(record.defs)
    if (definitions.length > MAX_DEFINITIONS || definitions.some(([name]) => !REFERENCE_NAME.test(name))) return false
  }
  if (typeof record.theme !== "object" || record.theme === null || Array.isArray(record.theme)) return false
  const theme = record.theme as Record<string, unknown>
  if (REQUIRED_THEME_ROLE_KEYS.some((key) => theme[key] === undefined)) return false
  const allowed = new Set<string>([...THEME_ROLE_KEYS, "thinkingOpacity"])
  if (Object.keys(theme).some((key) => !allowed.has(key))) return false
  try {
    resolveThemeJson(value as ThemeJson, "dark", "validation")
    resolveThemeJson(value as ThemeJson, "light", "validation")
    return true
  } catch {
    return false
  }
}

const customThemeJson = new Map<string, ThemeJson>()
const builtinThemeNames = Object.keys(DEFAULT_THEMES)

export const kennelTheme = resolveThemeJson(DEFAULT_THEMES.opencode!, "dark", "opencode")
export const daylightTheme = resolveThemeJson(DEFAULT_THEMES.opencode!, "light", "opencode")
export const tokyoNightTheme = resolveThemeJson(DEFAULT_THEMES.tokyonight!, "dark", "tokyonight")
export const catppuccinTheme = resolveThemeJson(DEFAULT_THEMES.catppuccin!, "dark", "catppuccin")
export const gruvboxTheme = resolveThemeJson(DEFAULT_THEMES.gruvbox!, "dark", "gruvbox")
export const nordTheme = resolveThemeJson(DEFAULT_THEMES.nord!, "dark", "nord")

export function systemThemeFor(mode: ThemeMode | null): RottweilerTheme {
  return resolveThemeJson(DEFAULT_THEMES.opencode!, mode === "light" ? "light" : "dark", "system")
}

/** Generate OpenCode's System theme from the terminal's real ANSI palette. */
export function systemThemeFromPalette(
  colors: TerminalThemeColors,
  fallbackMode: ThemeMode = "dark",
): RottweilerTheme {
  const background = colors.defaultBackground ?? colors.palette[0]
  const foreground = colors.defaultForeground ?? colors.palette[7]
  if (background === null || background === undefined || foreground === null || foreground === undefined) {
    return systemThemeFor(fallbackMode)
  }
  const bg = hexRgb(background)
  const mode = terminalModeFromPalette(colors) ?? fallbackMode
  const dark = mode === "dark"
  const color = (index: number, fallback: string) => colors.palette[index] ?? fallback
  const red = color(1, "#CD3131")
  const green = color(2, "#0DBC79")
  const yellow = color(3, "#E5E510")
  const blue = color(4, "#2472C8")
  const magenta = color(5, "#BC3FBC")
  const cyan = color(6, "#11A8CD")
  const redBright = color(9, "#F14C4C")
  const greenBright = color(10, "#23D18B")
  const grays = systemGrays(bg, dark)
  const muted = systemMuted(bg, dark)
  const diffAlpha = dark ? 0.22 : 0.14
  const roles: ThemeJson = {
    theme: {
      primary: cyan, secondary: magenta, accent: cyan,
      error: red, warning: yellow, success: green, info: cyan,
      text: foreground, textMuted: muted, selectedListItemText: background,
      background: "transparent", backgroundPanel: grays[2]!,
      backgroundElement: grays[3]!, backgroundMenu: grays[3]!,
      borderSubtle: grays[6]!, border: grays[7]!, borderActive: grays[8]!,
      diffAdded: green, diffRemoved: red, diffContext: grays[7]!, diffHunkHeader: grays[7]!,
      diffHighlightAdded: greenBright, diffHighlightRemoved: redBright,
      diffAddedBg: tintHex(background, green, diffAlpha),
      diffRemovedBg: tintHex(background, red, diffAlpha),
      diffContextBg: grays[2]!, diffLineNumber: muted,
      diffAddedLineNumberBg: tintHex(grays[2]!, green, diffAlpha),
      diffRemovedLineNumberBg: tintHex(grays[2]!, red, diffAlpha),
      markdownText: foreground, markdownHeading: foreground, markdownLink: blue,
      markdownLinkText: cyan, markdownCode: green, markdownBlockQuote: yellow,
      markdownEmph: yellow, markdownStrong: foreground, markdownHorizontalRule: grays[7]!,
      markdownListItem: blue, markdownListEnumeration: cyan, markdownImage: blue,
      markdownImageText: cyan, markdownCodeBlock: foreground,
      syntaxComment: muted, syntaxKeyword: magenta, syntaxFunction: blue,
      syntaxVariable: foreground, syntaxString: green, syntaxNumber: yellow,
      syntaxType: cyan, syntaxOperator: cyan, syntaxPunctuation: foreground,
    },
  }
  return resolveThemeJson(roles, mode, "system")
}

export function terminalModeFromPalette(colors: TerminalThemeColors): ThemeMode | null {
  const background = colors.defaultBackground ?? colors.palette[0]
  if (background === null || background === undefined) return null
  const { r, g, b } = hexRgb(background)
  return 0.299 * r + 0.587 * g + 0.114 * b > 127.5 ? "light" : "dark"
}

function hexRgb(value: string): { r: number; g: number; b: number } {
  const normalized = normalizeHex(value)
  const match = /^#([0-9A-F]{2})([0-9A-F]{2})([0-9A-F]{2})/.exec(normalized)
  if (match === null) return { r: 0, g: 0, b: 0 }
  return { r: Number.parseInt(match[1]!, 16), g: Number.parseInt(match[2]!, 16), b: Number.parseInt(match[3]!, 16) }
}

function hexFromRgb(r: number, g: number, b: number): HexColor {
  const channel = (value: number) => Math.max(0, Math.min(255, Math.round(value))).toString(16).padStart(2, "0")
  return `#${channel(r)}${channel(g)}${channel(b)}`
}

function tintHex(base: string, overlay: string, alpha: number): HexColor {
  const from = hexRgb(base)
  const to = hexRgb(overlay)
  return hexFromRgb(
    from.r + (to.r - from.r) * alpha,
    from.g + (to.g - from.g) * alpha,
    from.b + (to.b - from.b) * alpha,
  )
}

function systemGrays(background: { r: number; g: number; b: number }, dark: boolean): Record<number, HexColor> {
  const result: Record<number, HexColor> = {}
  const luminance = 0.299 * background.r + 0.587 * background.g + 0.114 * background.b
  for (let index = 1; index <= 12; index += 1) {
    const factor = index / 12
    if (dark && luminance < 10) {
      const value = factor * 0.4 * 255
      result[index] = hexFromRgb(value, value, value)
      continue
    }
    if (!dark && luminance > 245) {
      const value = 255 - factor * 0.4 * 255
      result[index] = hexFromRgb(value, value, value)
      continue
    }
    const target = dark ? luminance + (255 - luminance) * factor * 0.4 : luminance * (1 - factor * 0.4)
    const ratio = luminance === 0 ? 0 : target / luminance
    result[index] = hexFromRgb(background.r * ratio, background.g * ratio, background.b * ratio)
  }
  return result
}

function systemMuted(background: { r: number; g: number; b: number }, dark: boolean): HexColor {
  const luminance = 0.299 * background.r + 0.587 * background.g + 0.114 * background.b
  const value = dark
    ? luminance < 10 ? 180 : Math.min(Math.floor(160 + luminance * 0.3), 200)
    : luminance > 245 ? 75 : Math.max(Math.floor(100 - (255 - luminance) * 0.2), 60)
  return hexFromRgb(value, value, value)
}

export const systemTheme = systemThemeFor(terminalUsesLightBackground() ? "light" : "dark")

function terminalUsesLightBackground(): boolean {
  const background = process.env.COLORFGBG?.split(";").at(-1)
  return background !== undefined && /^\d+$/.test(background) && [7, 15].includes(Number(background))
}

const registeredThemes: RottweilerTheme[] = []
export const themeCatalog: readonly RottweilerTheme[] = registeredThemes

function refreshCatalog(mode: ThemeMode = "dark"): void {
  const resolved = new Map<string, RottweilerTheme>()
  resolved.set("system", systemThemeFor(mode))
  for (const name of builtinThemeNames) resolved.set(name, resolveThemeJson(DEFAULT_THEMES[name]!, mode, name))
  for (const [name, theme] of customThemeJson) resolved.set(name, resolveThemeJson(theme, mode, name))
  registeredThemes.splice(0, registeredThemes.length, ...resolved.values())
}
refreshCatalog()

export function themeCatalogFor(mode: ThemeMode): readonly RottweilerTheme[] {
  const themes = new Map<string, RottweilerTheme>()
  themes.set("system", systemThemeFor(mode))
  for (const name of builtinThemeNames) themes.set(name, resolveThemeJson(DEFAULT_THEMES[name]!, mode, name))
  for (const [name, theme] of customThemeJson) themes.set(name, resolveThemeJson(theme, mode, name))
  return [...themes.values()]
}

/** Load bounded, data-only OpenCode-schema themes. Files never execute and symlinks are rejected. */
export async function loadCustomThemes(directory: string): Promise<void> {
  let names: string[]
  try {
    names = (await readdir(directory)).filter((name) => name.endsWith(".json")).sort().slice(0, 64)
  } catch {
    customThemeJson.clear()
    refreshCatalog()
    return
  }
  const next = new Map<string, ThemeJson>()
  for (const file of names) {
    const name = basename(file, ".json")
    if (!THEME_NAME.test(name)) continue
    try {
      const path = join(directory, file)
      const metadata = await lstat(path)
      if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > 128 * 1024) continue
      const parsed: unknown = JSON.parse(await readFile(path, "utf8"))
      if (isThemeJson(parsed)) next.set(name, parsed)
    } catch {
      // Optional custom themes cannot block startup.
    }
  }
  customThemeJson.clear()
  for (const [name, theme] of next) customThemeJson.set(name, theme)
  refreshCatalog()
}

export function themeByName(name: string, mode: ThemeMode = "dark"): RottweilerTheme | undefined {
  if (name === "system") return systemThemeFor(mode)
  const source = customThemeJson.get(name) ?? DEFAULT_THEMES[name]
  return source === undefined ? undefined : resolveThemeJson(source, mode, name)
}

export function createSyntaxStyle(theme: RottweilerTheme): SyntaxStyle {
  return SyntaxStyle.fromStyles({
    // Markdown prose inherits this default capture. Keep body copy on the
    // theme's primary foreground; syntax-specific captures still color code.
    default: { fg: theme.text },
    prompt: { fg: theme.accent },
    "markup.heading": { fg: theme.markdownHeading, bold: true },
    "markup.heading.1": { fg: theme.markdownHeading, bold: true, underline: true },
    "markup.heading.2": { fg: theme.markdownHeading, bold: true },
    "markup.heading.3": { fg: theme.markdownHeading, bold: true },
    "markup.heading.4": { fg: theme.markdownHeading, bold: true },
    "markup.heading.5": { fg: theme.markdownHeading, bold: true },
    "markup.heading.6": { fg: theme.markdownHeading, bold: true },
    "markup.bold": { fg: theme.markdownStrong, bold: true },
    "markup.strong": { fg: theme.markdownStrong, bold: true },
    "markup.italic": { fg: theme.markdownEmph, italic: true },
    "markup.link": { fg: theme.markdownLink, underline: true },
    "markup.link.label": { fg: theme.markdownLinkText },
    "markup.link.url": { fg: theme.markdownLink },
    "markup.quote": { fg: theme.markdownBlockQuote },
    "markup.list": { fg: theme.markdownListItem },
    "markup.raw": { fg: theme.markdownCode },
    comment: { fg: theme.syntaxComment, italic: true },
    "comment.documentation": { fg: theme.syntaxComment, italic: true },
    string: { fg: theme.syntaxString },
    symbol: { fg: theme.syntaxString },
    number: { fg: theme.syntaxNumber },
    boolean: { fg: theme.syntaxNumber },
    keyword: { fg: theme.syntaxKeyword, italic: true },
    "keyword.type": { fg: theme.syntaxType, bold: true, italic: true },
    "keyword.function": { fg: theme.syntaxFunction },
    function: { fg: theme.syntaxFunction },
    "function.method": { fg: theme.syntaxFunction },
    type: { fg: theme.syntaxType },
    variable: { fg: theme.syntaxVariable },
    operator: { fg: theme.syntaxOperator },
    punctuation: { fg: theme.syntaxPunctuation },
  })
}
