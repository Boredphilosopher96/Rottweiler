import { SyntaxStyle } from "@opentui/core"
import { lstat, readFile, readdir } from "node:fs/promises"
import { join } from "node:path"

export interface RottweilerTheme {
  readonly name: string
  readonly background: string
  readonly panel: string
  readonly panelRaised: string
  readonly foreground: string
  readonly muted: string
  readonly subtle: string
  readonly accent: string
  readonly accentStrong: string
  readonly success: string
  readonly warning: string
  readonly danger: string
  readonly info: string
  readonly border: string
  readonly focus: string
  readonly selection: string
  readonly added: string
  readonly removed: string
}

export const kennelTheme: RottweilerTheme = {
  name: "kennel-dark",
  background: "#0B0D12",
  panel: "#11151D",
  panelRaised: "#171C26",
  foreground: "#E8ECF3",
  muted: "#A5AFC0",
  subtle: "#687386",
  accent: "#E6B450",
  accentStrong: "#F7C56B",
  success: "#7BD88F",
  warning: "#F7C56B",
  danger: "#FF6B6B",
  info: "#78DCE8",
  border: "#303849",
  focus: "#E6B450",
  selection: "#273449",
  added: "#173D2A",
  removed: "#48242B",
}

export const daylightTheme: RottweilerTheme = {
  name: "daylight",
  background: "#F7F5EF",
  panel: "#EEEAE0",
  panelRaised: "#E6E0D3",
  foreground: "#25221D",
  muted: "#625D53",
  subtle: "#8B8376",
  accent: "#9A5B00",
  accentStrong: "#7A4700",
  success: "#267A3F",
  warning: "#9A5B00",
  danger: "#B4232F",
  info: "#176B87",
  border: "#C7BFAF",
  focus: "#9A5B00",
  selection: "#D8E6ED",
  added: "#D6EFDC",
  removed: "#F2D8DC",
}

export const tokyoNightTheme: RottweilerTheme = {
  name: "tokyo-night",
  background: "#1A1B26", panel: "#202330", panelRaised: "#24283B",
  foreground: "#C0CAF5", muted: "#A9B1D6", subtle: "#565F89",
  accent: "#BB9AF7", accentStrong: "#7AA2F7", success: "#9ECE6A",
  warning: "#E0AF68", danger: "#F7768E", info: "#7DCFFF",
  border: "#3B4261", focus: "#7AA2F7", selection: "#283457",
  added: "#203A32", removed: "#3D2636",
}

export const catppuccinTheme: RottweilerTheme = {
  name: "catppuccin-mocha",
  background: "#1E1E2E", panel: "#252536", panelRaised: "#313244",
  foreground: "#CDD6F4", muted: "#A6ADC8", subtle: "#6C7086",
  accent: "#CBA6F7", accentStrong: "#89B4FA", success: "#A6E3A1",
  warning: "#F9E2AF", danger: "#F38BA8", info: "#89DCEB",
  border: "#45475A", focus: "#89B4FA", selection: "#363A55",
  added: "#263B32", removed: "#452C3A",
}

export const gruvboxTheme: RottweilerTheme = {
  name: "gruvbox",
  background: "#282828", panel: "#32302F", panelRaised: "#3C3836",
  foreground: "#EBDBB2", muted: "#BDAE93", subtle: "#928374",
  accent: "#D79921", accentStrong: "#FABD2F", success: "#B8BB26",
  warning: "#FE8019", danger: "#FB4934", info: "#83A598",
  border: "#504945", focus: "#FABD2F", selection: "#504945",
  added: "#344327", removed: "#4A2927",
}

export const nordTheme: RottweilerTheme = {
  name: "nord",
  background: "#2E3440", panel: "#343B49", panelRaised: "#3B4252",
  foreground: "#ECEFF4", muted: "#D8DEE9", subtle: "#7B88A1",
  accent: "#88C0D0", accentStrong: "#81A1C1", success: "#A3BE8C",
  warning: "#EBCB8B", danger: "#BF616A", info: "#8FBCBB",
  border: "#4C566A", focus: "#88C0D0", selection: "#434C5E",
  added: "#34453D", removed: "#49343B",
}

/** Follow the terminal's conventional COLORFGBG hint while retaining a stable setting name. */
export const systemTheme: RottweilerTheme = {
  ...systemThemeFor(terminalUsesLightBackground() ? "light" : "dark"),
  name: "system",
}

export function systemThemeFor(mode: "light" | "dark" | null): RottweilerTheme {
  return {
    ...(mode === "light" ? daylightTheme : kennelTheme),
    name: "system",
  }
}

function terminalUsesLightBackground(): boolean {
  const background = process.env.COLORFGBG?.split(";").at(-1)
  if (background === undefined || !/^\d+$/.test(background)) return false
  const paletteIndex = Number(background)
  return paletteIndex === 7 || paletteIndex === 15
}

/** Complete built-in theme catalog consumed by startup and the live picker. */
const builtinThemes: readonly RottweilerTheme[] = [
  systemTheme,
  kennelTheme,
  daylightTheme,
  tokyoNightTheme,
  catppuccinTheme,
  gruvboxTheme,
  nordTheme,
]
const registeredThemes: RottweilerTheme[] = [...builtinThemes]
export const themeCatalog: readonly RottweilerTheme[] = registeredThemes

const THEME_COLOR_KEYS = [
  "background", "panel", "panelRaised", "foreground", "muted", "subtle",
  "accent", "accentStrong", "success", "warning", "danger", "info", "border",
  "focus", "selection", "added", "removed",
] as const

/** Load bounded, data-only custom themes. Invalid files are ignored, never executed. */
export async function loadCustomThemes(directory: string): Promise<void> {
  let names: string[]
  try {
    names = (await readdir(directory)).filter((name) => name.endsWith(".json")).sort().slice(0, 64)
  } catch {
    return
  }
  const custom: RottweilerTheme[] = []
  for (const file of names) {
    const path = join(directory, file)
    try {
      const metadata = await lstat(path)
      if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > 32 * 1024) continue
      const parsed: unknown = JSON.parse(await readFile(path, "utf8"))
      const theme = validatedTheme(parsed)
      if (theme !== null && !builtinThemes.some((builtin) => builtin.name === theme.name)) custom.push(theme)
    } catch {
      // A malformed optional theme must not prevent the coding client starting.
    }
  }
  registeredThemes.splice(0, registeredThemes.length, ...builtinThemes, ...custom)
}

function validatedTheme(value: unknown): RottweilerTheme | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null
  const record = value as Record<string, unknown>
  if (typeof record.name !== "string" || !/^[a-z0-9][a-z0-9._-]{0,63}$/.test(record.name)) return null
  for (const key of THEME_COLOR_KEYS) {
    if (typeof record[key] !== "string" || !/^#[0-9a-fA-F]{6}$/.test(record[key])) return null
  }
  return Object.fromEntries([
    ["name", record.name],
    ...THEME_COLOR_KEYS.map((key) => [key, record[key]]),
  ]) as unknown as RottweilerTheme
}

export function themeByName(name: string): RottweilerTheme | undefined {
  return themeCatalog.find((theme) => theme.name === name)
}

export function createSyntaxStyle(theme: RottweilerTheme): SyntaxStyle {
  return SyntaxStyle.fromStyles({
    default: { fg: theme.foreground },
    "markup.heading": { fg: theme.accentStrong, bold: true },
    "markup.heading.1": { fg: theme.accentStrong, bold: true, underline: true },
    "markup.heading.2": { fg: theme.accentStrong, bold: true },
    "markup.heading.3": { fg: theme.accentStrong, bold: true },
    "markup.heading.4": { fg: theme.accentStrong, bold: true },
    "markup.heading.5": { fg: theme.accentStrong, bold: true },
    "markup.heading.6": { fg: theme.accentStrong, bold: true },
    "markup.bold": { fg: theme.foreground, bold: true },
    "markup.strong": { fg: theme.foreground, bold: true },
    "markup.italic": { fg: theme.foreground, italic: true },
    "markup.link": { fg: theme.info, underline: true },
    "markup.link.label": { fg: theme.info },
    "markup.link.url": { fg: theme.subtle },
    "markup.quote": { fg: theme.muted },
    "markup.list": { fg: theme.accent },
    "markup.raw": { fg: theme.success },
    comment: { fg: theme.subtle, italic: true },
    string: { fg: theme.success },
    number: { fg: theme.accentStrong },
    keyword: { fg: theme.info, bold: true },
    function: { fg: theme.accentStrong },
    type: { fg: theme.info },
    variable: { fg: theme.foreground },
    operator: { fg: theme.muted },
    punctuation: { fg: theme.muted },
  })
}
