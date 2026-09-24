import { EXTENSIONS_SECTION, type CommandEntry } from "../command-palette"
import { formatKeycap, isKeybindingAction, type CompiledKeybindings } from "../keybindings"
import type { SessionActionKind } from "../protocol"
import {
  BUILTIN_COMMANDS,
  catalogCommand,
  commandSourceLabel,
  type CatalogCommand,
  type CatalogCommandName,
  type CommandChoice,
} from "../session-commands"
import type { RottweilerState } from "../state"

/** What running one command entry does. */
export type CommandAction =
  | { readonly kind: "builtin"; readonly name: CatalogCommandName }
  | { readonly kind: "extension"; readonly name: string; readonly requiresArgument: boolean }
  | { readonly kind: "retry" }

export type PaletteAction = CommandEntry<CommandAction>

export interface CommandCatalogInput {
  readonly state: RottweilerState
  readonly bindings: CompiledKeybindings
  /** This session has or had child agents. */
  readonly childAgents: boolean
  /** Availability projection status for the live catalog request. */
  readonly availability: "ready" | "checking" | "failed"
  /** Error text when the live extension catalog failed. */
  readonly catalogError: string | null
}

/**
 * Projects the engine-owned catalog and live extension descriptors into one
 * list. Every action appears exactly once; aliases are search terms only.
 */
export function commandEntries(input: CommandCatalogInput): readonly PaletteAction[] {
  const entries: PaletteAction[] = []
  for (const command of BUILTIN_COMMANDS) {
    if (!visible(command, input)) continue
    entries.push({
      id: `cmd.${command.name}`,
      name: command.name,
      aliases: command.aliases,
      title: command.title,
      section: command.section,
      description: command.description,
      argumentHint: command.argument_hint,
      keycap: keycap(command, input.bindings),
      sourceLabel: null,
      unavailableReason: unavailableReason(command.availability, input),
      action: { kind: "builtin", name: command.name },
    })
  }
  for (const command of extensionCommands(input.state.commands)) {
    const argumentHint = command.usage.startsWith(`/${command.name}`)
      ? command.usage.slice(command.name.length + 1).trim()
      : ""
    entries.push({
      id: `ext.${command.name}`,
      name: command.name,
      aliases: [],
      title: `/${command.name}`,
      section: EXTENSIONS_SECTION,
      description: command.description,
      argumentHint,
      keycap: null,
      sourceLabel: extensionSourceLabel(command),
      unavailableReason: null,
      action: { kind: "extension", name: command.name, requiresArgument: /<[^>]+>/u.test(argumentHint) },
    })
  }
  if (input.catalogError !== null) {
    entries.push({
      id: "ext.retry",
      name: "retry",
      aliases: [],
      title: "Retry loading extension commands",
      section: EXTENSIONS_SECTION,
      description: input.catalogError,
      argumentHint: "",
      keycap: null,
      sourceLabel: null,
      unavailableReason: null,
      action: { kind: "retry" },
    })
  }
  return entries
}

/**
 * Palette provenance for one extension row: the artifact kind followed by its
 * discovery scope when the engine reports one, for example `skill · user`.
 */
export function extensionSourceLabel(command: Pick<CommandChoice, "name" | "source" | "scope">): string {
  const scope = command.scope ?? null
  switch (command.source) {
    case "project":
    case "user":
      return `command · ${scope ?? command.source}`
    case "skill":
      return scope === null ? "skill" : `skill · ${scope}`
    default:
      return commandSourceLabel(command)
  }
}

/** Live descriptors that are not built-in catalog entries, in stable name order. */
export function extensionCommands(commands: readonly CommandChoice[]): readonly CommandChoice[] {
  return commands
    .filter((command) => catalogCommand(command.name) === undefined)
    .sort((left, right) => left.name.localeCompare(right.name))
}

function visible(command: CatalogCommand, input: CommandCatalogInput): boolean {
  switch (command.visibility) {
    case "always": return true
    case "queued_messages": return input.state.queuedMessages.length > 0
    case "child_agents": return input.childAgents
    case "errors": return input.state.errorHistory.length > 0 || input.state.errors.length > 0
  }
}

function keycap(command: CatalogCommand, bindings: CompiledKeybindings): string | null {
  const action = command.keybinding
  if (action === null || !isKeybindingAction(action)) return null
  for (const [stroke, bound] of bindings.bindings("global")) {
    if (bound === action) return formatKeycap(stroke)
  }
  return null
}

/**
 * Only an engine-projected refusal disables an entry. A refresh keeps the last
 * projection; when none is known the engine still rechecks every action on
 * submission, so entries stay usable instead of flashing a transient reason.
 */
function unavailableReason(kind: SessionActionKind | null, input: CommandCatalogInput): string | null {
  if (kind === null || input.availability === "failed") return null
  return input.state.availableActions.find((entry) => entry.action === kind)?.unavailable_reason ?? null
}

/** Detail copy: description plus usage, queue behavior, and recovery. */
export function commandDetail(entry: PaletteAction, state: RottweilerState): string {
  const usage = entry.action.kind === "retry"
    ? ""
    : `\n\n/${entry.name}${entry.argumentHint.length === 0 ? "" : ` ${entry.argumentHint}`}`
  const aliases = entry.aliases.length === 0 ? "" : `\nAlso: ${entry.aliases.map((alias) => `/${alias}`).join(", ")}`
  if (entry.unavailableReason !== null) return `${entry.unavailableReason}${usage}`
  const command = entry.action.kind === "builtin" ? catalogCommand(entry.action.name) : undefined
  const queued = command?.availability != null
    && state.availableActions.find((action) => action.action === command.availability)?.queued === true
  return `${entry.description}${queued ? " · Queues until the current work finishes" : ""}${usage}${aliases}`
}
