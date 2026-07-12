import { SyntaxStyle } from "@opentui/core"

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

/** Complete built-in theme catalog consumed by startup and the live picker. */
export const themeCatalog: readonly RottweilerTheme[] = [kennelTheme, daylightTheme]

export function themeByName(name: string): RottweilerTheme | undefined {
  return themeCatalog.find((theme) => theme.name === name)
}

export function createSyntaxStyle(theme: RottweilerTheme): SyntaxStyle {
  return SyntaxStyle.fromStyles({
    default: { fg: theme.foreground },
    "markup.heading": { fg: theme.accentStrong, bold: true },
    "markup.bold": { fg: theme.foreground, bold: true },
    "markup.italic": { fg: theme.foreground, italic: true },
    "markup.link": { fg: theme.info, underline: true },
    "markup.link.label": { fg: theme.info },
    "markup.link.url": { fg: theme.subtle },
    "markup.quote": { fg: theme.muted, italic: true },
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
