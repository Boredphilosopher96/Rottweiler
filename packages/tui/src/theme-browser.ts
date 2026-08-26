import { fuzzyMatch, type FuzzyMatch } from "./components/picker"
import {
  DEFAULT_THEMES,
  THEME_ROLE_KEYS,
  type RottweilerTheme,
  type ThemeRole,
} from "./theme"

export type ThemeBrowserSource = "system" | "builtin" | "custom"

export interface ThemeBrowserSwatch {
  readonly role: "background" | "primary" | "accent" | "success" | "error"
  readonly color: string
}

export interface ThemeBrowserRow {
  readonly id: `theme:${string}`
  readonly name: string
  readonly theme: RottweilerTheme
  readonly mode: RottweilerTheme["mode"]
  readonly source: ThemeBrowserSource
  readonly active: boolean
  readonly matchSpans: readonly (readonly [start: number, end: number])[]
  readonly swatches: readonly [
    ThemeBrowserSwatch,
    ThemeBrowserSwatch,
    ThemeBrowserSwatch,
    ThemeBrowserSwatch,
    ThemeBrowserSwatch,
  ]
}

export type ThemeBrowserDetail =
  | {
      readonly kind: "theme"
      readonly name: string
      readonly mode: RottweilerTheme["mode"]
      readonly roleCount: number
      readonly roles: Readonly<Record<ThemeRole, string>>
      readonly theme: RottweilerTheme
    }
  | { readonly kind: "empty"; readonly message: "No matching themes" }

export interface ThemeBrowserCounts {
  readonly visible: number
  readonly total: number
  readonly custom: number
}

export interface ThemeBrowserModel {
  readonly title: "THEME"
  readonly query: string
  readonly rows: readonly ThemeBrowserRow[]
  readonly selectedId: `theme:${string}` | null
  readonly detail: ThemeBrowserDetail
  readonly counts: ThemeBrowserCounts
  readonly status: string
  readonly footer: "arrows preview · Enter apply · Esc cancel"
  readonly customThemeDirectory: "~/.rottweiler/themes/"
}

interface ThemeBrowserModelOptions {
  readonly themes: readonly RottweilerTheme[]
  readonly query: string
  readonly selectedName: string | null
  readonly currentName: string
}

export function createThemeBrowserModel(
  options: ThemeBrowserModelOptions,
): ThemeBrowserModel {
  const query = options.query.trim()
  const candidates = options.themes.flatMap((theme) => {
    const match = fuzzyMatch(query, theme.name)
    return query.length > 0 && match === null ? [] : [{ theme, match }]
  })
  const visibleThemes = candidates.map(({ theme }) => theme)
  const selected =
    visibleThemes.find((theme) => theme.name === options.selectedName) ??
    visibleThemes.find((theme) => theme.name === options.currentName) ??
    visibleThemes[0]
  const custom = options.themes.filter((theme) => themeSource(theme.name) === "custom").length
  const mode = selected?.mode ?? options.themes[0]?.mode ?? "dark"
  const counts = {
    visible: visibleThemes.length,
    total: options.themes.length,
    custom,
  }

  return {
    title: "THEME",
    query,
    rows: candidates.map(({ theme, match }) => ({
      id: `theme:${theme.name}`,
      name: theme.name,
      theme,
      mode: theme.mode,
      source: themeSource(theme.name),
      active: theme.name === options.currentName,
      matchSpans: matchSpans(match),
      swatches: [
        { role: "background", color: theme.background },
        { role: "primary", color: theme.primary },
        { role: "accent", color: theme.accent },
        { role: "success", color: theme.success },
        { role: "error", color: theme.error },
      ],
    })),
    selectedId: selected === undefined ? null : `theme:${selected.name}`,
    detail: selected === undefined
      ? { kind: "empty", message: "No matching themes" }
      : {
          kind: "theme",
          name: selected.name,
          mode: selected.mode,
          roleCount: THEME_ROLE_KEYS.length,
          roles: Object.fromEntries(
            THEME_ROLE_KEYS.map((role) => [role, selected[role]]),
          ) as Readonly<Record<ThemeRole, string>>,
          theme: selected,
        },
    counts,
    status: query.length === 0
      ? `${counts.total} themes · ${mode} · ${counts.custom} custom`
      : `${counts.visible} of ${counts.total} themes · ${mode} · ${counts.custom} custom`,
    footer: "arrows preview · Enter apply · Esc cancel",
    customThemeDirectory: "~/.rottweiler/themes/",
  }
}

function themeSource(name: string): ThemeBrowserSource {
  if (name === "system") return "system"
  return Object.hasOwn(DEFAULT_THEMES, name) ? "builtin" : "custom"
}

function matchSpans(
  match: FuzzyMatch | null,
): readonly (readonly [start: number, end: number])[] {
  if (match === null) return []
  const spans: Array<readonly [number, number]> = []
  for (const position of match.positions) {
    const previous = spans.at(-1)
    if (previous !== undefined && previous[1] === position) {
      spans[spans.length - 1] = [previous[0], position + 1]
    } else {
      spans.push([position, position + 1])
    }
  }
  return spans
}
