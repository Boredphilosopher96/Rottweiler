import type { KeyEvent } from "@opentui/core"

export type KeybindingPreset = "standard" | "vim"
export type InputMode = "standard" | "normal" | "insert"
export type VimFocus = "composer" | "transcript" | "picker"

export type KeybindingContext =
  | "global"
  | "standard"
  | "vim_normal"
  | "vim_insert"
  | "picker_normal"
  | "picker_insert"
  | "review"

export type KeybindingAction =
  | "append_insert"
  | "close_overlay"
  | "cycle_agent_mode"
  | "delete_character"
  | "enter_insert"
  | "enter_normal"
  | "focus_next"
  | "focus_previous"
  | "line_end"
  | "line_start"
  | "move_down"
  | "move_left"
  | "move_right"
  | "move_up"
  | "open_command_picker"
  | "open_external_editor"
  | "open_mode_picker"
  | "open_model_picker"
  | "open_review"
  | "open_session_picker"
  | "open_subagent_picker"
  | "page_down"
  | "page_up"
  | "paste_image"
  | "select_current"
  | "view_bottom"
  | "view_top"
  | "word_backward"
  | "word_forward"

export interface KeybindingConfiguration {
  readonly preset?: KeybindingPreset
  /** Action-to-keystroke overrides. An empty array explicitly unbinds an action. */
  readonly bindings?: Readonly<
    Partial<
      Record<
        KeybindingContext,
        Readonly<Partial<Record<KeybindingAction, string | readonly string[]>>>
      >
    >
  >
}

export interface CompiledKeybindings {
  readonly preset: KeybindingPreset
  resolve(context: KeybindingContext, event: KeyEvent): KeybindingAction | null
  bindings(context: KeybindingContext): ReadonlyMap<string, KeybindingAction>
}

/**
 * Ask supporting terminals to encode every physical key as an extended event.
 *
 * `disambiguate` alone does not stop every macOS terminal from applying its
 * Cocoa Command+Right -> Ctrl+E compatibility mapping before forwarding input.
 * Reporting all keys as escape sequences preserves the physical Super+Right
 * modifier, while physical Ctrl+E remains an unambiguous Kitty event.
 */
export const enhancedKeyboardOptions = Object.freeze({ allKeysAsEscapes: true })

export class KeybindingConfigurationError extends Error {
  readonly issues: readonly string[]

  constructor(issues: readonly string[]) {
    super(`invalid TUI keybindings:\n${issues.map((issue) => `- ${issue}`).join("\n")}`)
    this.name = "KeybindingConfigurationError"
    this.issues = issues
  }
}

const CONTEXTS: readonly KeybindingContext[] = [
  "global",
  "standard",
  "vim_normal",
  "vim_insert",
  "picker_normal",
  "picker_insert",
  "review",
]

const ACTIONS: readonly KeybindingAction[] = [
  "append_insert",
  "close_overlay",
  "cycle_agent_mode",
  "delete_character",
  "enter_insert",
  "enter_normal",
  "focus_next",
  "focus_previous",
  "line_end",
  "line_start",
  "move_down",
  "move_left",
  "move_right",
  "move_up",
  "open_command_picker",
  "open_external_editor",
  "open_mode_picker",
  "open_model_picker",
  "open_review",
  "open_session_picker",
  "open_subagent_picker",
  "page_down",
  "page_up",
  "paste_image",
  "select_current",
  "view_bottom",
  "view_top",
  "word_backward",
  "word_forward",
]

/** Short, user-facing descriptions for every configurable action. */
export const KEYBINDING_ACTION_LABELS: Record<KeybindingAction, string> = {
  append_insert: "Enter insert mode after the cursor",
  close_overlay: "Close the current overlay",
  cycle_agent_mode: "Cycle agent mode",
  delete_character: "Delete character",
  enter_insert: "Enter insert mode",
  enter_normal: "Leave insert mode",
  focus_next: "Focus next panel",
  focus_previous: "Focus previous panel",
  line_end: "Move to line end",
  line_start: "Move to line start",
  move_down: "Move down",
  move_left: "Move left",
  move_right: "Move right",
  move_up: "Move up",
  open_command_picker: "Open command palette",
  open_external_editor: "Open external editor",
  open_mode_picker: "Switch mode",
  open_model_picker: "Switch model",
  open_review: "Review changes",
  open_session_picker: "Switch session",
  open_subagent_picker: "Open child agents",
  page_down: "Scroll transcript down",
  page_up: "Scroll transcript up",
  paste_image: "Paste image",
  select_current: "Select current item",
  view_bottom: "Jump to transcript bottom",
  view_top: "Jump to transcript top",
  word_backward: "Move word backward",
  word_forward: "Move word forward",
}

const STANDARD_DEFAULTS = {
  global: {
    open_review: ["ctrl+r"],
    cycle_agent_mode: ["shift+tab"],
    open_command_picker: ["ctrl+p"],
    open_model_picker: ["ctrl+m"],
    open_mode_picker: ["ctrl+o"],
    open_session_picker: ["ctrl+s"],
    open_subagent_picker: ["ctrl+g"],
    paste_image: ["ctrl+v"],
  },
  standard: {
    close_overlay: ["escape"],
    open_external_editor: ["ctrl+e"],
    page_up: ["pageup"],
    page_down: ["pagedown"],
    view_top: ["shift+pageup"],
    view_bottom: ["shift+pagedown"],
    // OpenTUI reports Command as `super` under Kitty keyboard and as `meta`
    // under some older CSI-u implementations. Own both shapes before the
    // textarea can reinterpret either one.
    line_start: ["super+left", "meta+left"],
    line_end: ["super+right", "meta+right"],
  },
  review: { close_overlay: ["escape"] },
} satisfies Partial<Record<KeybindingContext, Partial<Record<KeybindingAction, readonly string[]>>>>

const VIM_DEFAULTS = {
  global: {
    open_review: ["ctrl+r"],
    cycle_agent_mode: ["shift+tab"],
    open_command_picker: ["ctrl+p"],
    open_model_picker: ["ctrl+m"],
    open_mode_picker: ["ctrl+o"],
    open_session_picker: ["ctrl+s"],
    open_subagent_picker: ["ctrl+g"],
    paste_image: ["ctrl+v"],
  },
  vim_insert: {
    enter_normal: ["escape", "ctrl+["],
    open_external_editor: ["ctrl+e"],
  },
  vim_normal: {
    enter_insert: ["i"],
    append_insert: ["a"],
    move_left: ["h", "left"],
    move_down: ["j", "down"],
    move_up: ["k", "up"],
    move_right: ["l", "right"],
    word_forward: ["w"],
    word_backward: ["b"],
    line_start: ["0"],
    line_end: ["$"],
    delete_character: ["x"],
    page_down: ["ctrl+d"],
    page_up: ["ctrl+u"],
    view_top: ["g"],
    view_bottom: ["shift+g"],
    focus_next: ["tab"],
    open_command_picker: [":"],
  },
  picker_insert: { enter_normal: ["escape", "ctrl+["] },
  picker_normal: {
    close_overlay: ["escape"],
    enter_insert: ["i", "/"],
    move_down: ["j", "down"],
    move_up: ["k", "up"],
    view_top: ["g"],
    view_bottom: ["shift+g"],
    select_current: ["return"],
  },
  review: { close_overlay: ["escape"] },
} satisfies Partial<Record<KeybindingContext, Partial<Record<KeybindingAction, readonly string[]>>>>

const MAX_BINDINGS_PER_CONTEXT = 128
const MAX_KEYSTROKE_LENGTH = 48
const SAFETY_PANEL_KEYS = new Set([
  "a",
  "shift+a",
  "r",
  "shift+r",
  "j",
  "k",
  "up",
  "down",
  "shift+up",
  "shift+down",
  "return",
  "escape",
])
const MODIFIER_ORDER = ["ctrl", "meta", "super", "hyper", "alt", "shift"] as const
const MODIFIERS = new Set<string>(MODIFIER_ORDER)
const NAMED_KEYS = new Set([
  "backspace",
  "delete",
  "down",
  "end",
  "escape",
  "home",
  "left",
  "linefeed",
  "pagedown",
  "pageup",
  "return",
  "right",
  "space",
  "tab",
  "up",
])

/** Validate and compile a bounded keybinding table once at startup. */
export function compileKeybindings(
  configuration: KeybindingConfiguration = {},
): CompiledKeybindings {
  const issues: string[] = []
  if (typeof configuration !== "object" || configuration === null || Array.isArray(configuration)) {
    throw new KeybindingConfigurationError(["configuration must be a table"])
  }
  const preset = configuration.preset ?? "standard"
  if (preset !== "standard" && preset !== "vim") {
    issues.push(`preset must be "standard" or "vim", received ${JSON.stringify(preset)}`)
  }
  validateConfigurationShape(configuration, issues)

  const defaults = preset === "vim" ? VIM_DEFAULTS : STANDARD_DEFAULTS
  const actionTables = new Map<KeybindingContext, Map<KeybindingAction, readonly string[]>>()
  for (const context of CONTEXTS) {
    const actions = new Map<KeybindingAction, readonly string[]>()
    const defaultContext = defaults[context as keyof typeof defaults] as
      | Partial<Record<KeybindingAction, readonly string[]>>
      | undefined
    for (const [action, strokes] of Object.entries(defaultContext ?? {})) {
      actions.set(action as KeybindingAction, strokes)
    }
    const overrides = configuration.bindings?.[context]
    if (overrides !== undefined) {
      for (const [action, rawStrokes] of Object.entries(overrides)) {
        if (!ACTIONS.includes(action as KeybindingAction)) continue
        const strokes = typeof rawStrokes === "string" ? [rawStrokes] : rawStrokes
        if (Array.isArray(strokes)) actions.set(action as KeybindingAction, strokes)
      }
    }
    actionTables.set(context, actions)
  }

  const tables = new Map<KeybindingContext, ReadonlyMap<string, KeybindingAction>>()
  for (const context of CONTEXTS) {
    const table = new Map<string, KeybindingAction>()
    let bindingCount = 0
    for (const [action, strokes] of actionTables.get(context) ?? []) {
      for (const rawStroke of strokes) {
        bindingCount += 1
        if (bindingCount > MAX_BINDINGS_PER_CONTEXT) {
          issues.push(`${context} has more than ${MAX_BINDINGS_PER_CONTEXT} bindings`)
          break
        }
        const stroke = canonicalizeKeyStroke(rawStroke, `${context}.${action}`, issues)
        if (stroke === null) continue
        if (stroke === "ctrl+c") {
          issues.push(
            `${context}.${action} cannot bind "ctrl+c" because the renderer owns it for immediate exit`,
          )
          continue
        }
        if (
          (context === "global" || context === "review") &&
          SAFETY_PANEL_KEYS.has(stroke) &&
          !(context === "review" && action === "close_overlay" && stroke === "escape")
        ) {
          issues.push(
            `${context}.${action} cannot bind ${JSON.stringify(stroke)} because a focused safety panel owns it`,
          )
          continue
        }
        const conflictingAction = table.get(stroke)
        if (conflictingAction !== undefined && conflictingAction !== action) {
          issues.push(
            `${context} binds ${JSON.stringify(stroke)} to both ${conflictingAction} and ${action}`,
          )
          continue
        }
        table.set(stroke, action)
      }
    }
    tables.set(context, table)
  }
  const global = tables.get("global") ?? new Map()
  for (const context of CONTEXTS) {
    if (context === "global") continue
    for (const [stroke, action] of tables.get(context) ?? []) {
      const globalAction = global.get(stroke)
      if (globalAction !== undefined && globalAction !== action) {
        issues.push(
          `${context}.${action} uses ${JSON.stringify(stroke)}, which is shadowed by global.${globalAction}`,
        )
      }
    }
  }
  if (issues.length > 0) throw new KeybindingConfigurationError([...new Set(issues)])

  return {
    preset,
    resolve(context, event) {
      return tables.get(context)?.get(keyStrokeFromEvent(event)) ?? null
    },
    bindings(context) {
      return tables.get(context) ?? new Map()
    },
  }
}

/** Parse the documented TOML shape without allowing unbounded inline config. */
export function parseKeybindingToml(source: string): KeybindingConfiguration {
  if (new TextEncoder().encode(source).byteLength > 64 * 1024) {
    throw new KeybindingConfigurationError(["TOML input exceeds 64 KiB"])
  }
  let parsed: unknown
  try {
    parsed = Bun.TOML.parse(source)
  } catch (error) {
    throw new KeybindingConfigurationError([
      `TOML could not be parsed: ${error instanceof Error ? error.message : "unknown parse error"}`,
    ])
  }
  const configuration = parsed as KeybindingConfiguration
  compileKeybindings(configuration)
  return configuration
}

export function keyStrokeFromEvent(event: KeyEvent): string {
  const name = canonicalKeyName(event.name)
  const modifiers = [
    ...(event.ctrl ? ["ctrl"] : []),
    ...(event.meta ? ["meta"] : []),
    ...(event.super ? ["super"] : []),
    ...(event.hyper ? ["hyper"] : []),
    ...(event.option ? ["alt"] : []),
    ...(event.shift ? ["shift"] : []),
  ]
  return [...modifiers, name].join("+")
}

/** Render a canonical key stroke as the keycap copy shown throughout the TUI. */
export function formatKeycap(stroke: string): string {
  const labels: Readonly<Record<string, string>> = {
    alt: "Alt",
    ctrl: "Ctrl",
    meta: "Meta",
    super: "Super",
    hyper: "Hyper",
    shift: "Shift",
    escape: "Escape",
    return: "Enter",
    pageup: "PageUp",
    pagedown: "PageDown",
    space: "Space",
    tab: "Tab",
  }
  return stroke
    .split("+")
    .map((part) => labels[part] ?? part.toLocaleUpperCase())
    .join("+")
}

/**
 * Legacy macOS terminals encode Command+Left/Right as the same raw Ctrl+A/E
 * bytes produced by physical Control+A/E. There is no modifier bit left to
 * inspect at that point. Prefer safe cursor navigation for that ambiguous raw
 * shape; terminals that accepted enhanced keyboard negotiation still report a
 * physical Ctrl+E with `source: "kitty"`, so it continues to open $EDITOR.
 */
export function legacyMacNavigationAction(
  event: Pick<
    KeyEvent,
    | "ctrl"
    | "meta"
    | "super"
    | "hyper"
    | "option"
    | "shift"
    | "name"
    | "raw"
    | "source"
  >,
  platform: NodeJS.Platform = process.platform,
): Extract<KeybindingAction, "line_start" | "line_end"> | null {
  if (platform !== "darwin" || event.source !== "raw") return null
  if (
    event.ctrl &&
    !event.meta &&
    !event.super &&
    !event.hyper &&
    !event.option &&
    !event.shift
  ) {
    if (event.name === "a" && event.raw === "\u0001") return "line_start"
    if (event.name === "e" && event.raw === "\u0005") return "line_end"
  }
  return null
}

function validateConfigurationShape(configuration: KeybindingConfiguration, issues: string[]): void {
  if (typeof configuration !== "object" || configuration === null || Array.isArray(configuration)) {
    issues.push("configuration must be a table")
    return
  }
  for (const key of Object.keys(configuration)) {
    if (key !== "preset" && key !== "bindings") issues.push(`unknown top-level key ${JSON.stringify(key)}`)
  }
  const bindings = configuration.bindings
  if (bindings === undefined) return
  if (typeof bindings !== "object" || bindings === null || Array.isArray(bindings)) {
    issues.push("bindings must be a table")
    return
  }
  for (const [context, actions] of Object.entries(bindings)) {
    if (!CONTEXTS.includes(context as KeybindingContext)) {
      issues.push(`unknown keybinding context ${JSON.stringify(context)}`)
      continue
    }
    if (typeof actions !== "object" || actions === null || Array.isArray(actions)) {
      issues.push(`${context} must be a table`)
      continue
    }
    for (const [action, strokes] of Object.entries(actions)) {
      if (!ACTIONS.includes(action as KeybindingAction)) {
        issues.push(`${context} has unknown action ${JSON.stringify(action)}`)
      }
      if (
        typeof strokes !== "string" &&
        (!Array.isArray(strokes) || strokes.some((stroke) => typeof stroke !== "string"))
      ) {
        issues.push(`${context}.${action} must be a string or an array of strings`)
      }
    }
  }
}

function canonicalizeKeyStroke(raw: string, label: string, issues: string[]): string | null {
  if (raw.length === 0 || raw.length > MAX_KEYSTROKE_LENGTH || raw.trim() !== raw) {
    issues.push(`${label} contains an empty, padded, or overlong keystroke`)
    return null
  }
  const parts = raw.toLocaleLowerCase().split("+")
  if (parts.some((part) => part.length === 0)) {
    issues.push(`${label} contains malformed keystroke ${JSON.stringify(raw)}`)
    return null
  }
  const rawName = parts.pop()
  if (rawName === undefined) return null
  const modifiers = new Set<string>()
  for (const modifier of parts) {
    const normalized = modifier === "option" ? "alt" : modifier
    if (!MODIFIERS.has(normalized)) {
      issues.push(`${label} has unknown modifier ${JSON.stringify(modifier)}`)
      return null
    }
    if (modifiers.has(normalized)) {
      issues.push(`${label} repeats modifier ${JSON.stringify(modifier)}`)
      return null
    }
    modifiers.add(normalized)
  }
  const name = canonicalKeyName(rawName)
  if (!isSupportedKeyName(name)) {
    issues.push(`${label} has unsupported key name ${JSON.stringify(rawName)}`)
    return null
  }
  return [...MODIFIER_ORDER.filter((modifier) => modifiers.has(modifier)), name].join("+")
}

function canonicalKeyName(name: string): string {
  const normalized = name.toLocaleLowerCase()
  if (normalized === "esc") return "escape"
  if (normalized === "enter" || normalized === "linefeed") return "return"
  return normalized
}

function isSupportedKeyName(name: string): boolean {
  return (
    NAMED_KEYS.has(name) ||
    /^f(?:[1-9]|1[0-2])$/.test(name) ||
    /^[a-z0-9]$/.test(name) ||
    /^[\[\]{}()<>/:;,.!?@#$%^&*_=+`~'"\\|-]$/.test(name)
  )
}
