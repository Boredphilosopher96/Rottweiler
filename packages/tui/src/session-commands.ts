import { COMMAND_CATALOG } from "../../../protocol/types"
import type { RottweilerState } from "./state"

export type CommandChoice = RottweilerState["commands"][number]
export type CatalogCommand = (typeof COMMAND_CATALOG)[number]
export type CatalogCommandName = CatalogCommand["name"]
/** Catalog entries an interactive client opens as its own screen. */
export type CatalogScreenName = Extract<CatalogCommand, { readonly target: "screen" | "screen_or_engine" }>["name"]

/** Engine-owned built-in catalog, projected at build time. */
export const BUILTIN_COMMANDS: readonly CatalogCommand[] = COMMAND_CATALOG

/** Resolves a canonical name or alias to its catalog entry. */
export function catalogCommand(name: string): CatalogCommand | undefined {
  return BUILTIN_COMMANDS.find((command) =>
    command.name === name || (command.aliases as readonly string[]).includes(name))
}

export function catalogUsage(command: CatalogCommand): string {
  return command.argument_hint.length === 0 ? `/${command.name}` : `/${command.name} ${command.argument_hint}`
}

export type SlashResolution =
  /** An interactive screen opened by the client. */
  | { readonly type: "screen"; readonly name: CatalogScreenName }
  /** A built-in engine invocation, rewritten to its canonical command name. */
  | { readonly type: "engine"; readonly content: string }
  | { readonly type: "invalid"; readonly message: string }

/**
 * Classifies composer input against the built-in catalog. Returns null for
 * ordinary prompts and extension commands, which the engine resolves itself.
 */
export function resolveSlashInput(content: string): SlashResolution | null {
  const match = /^\s*\/(\S+)(?:\s+([\s\S]*))?$/u.exec(content)
  if (match === null) return null
  const command = catalogCommand(match[1] ?? "")
  if (command === undefined) return null
  const args = (match[2] ?? "").trim()
  if (args.length === 0 && command.target !== "engine") {
    return { type: "screen", name: command.name as CatalogScreenName }
  }
  if (command.target === "screen") {
    return { type: "invalid", message: `usage: /${command.name}` }
  }
  return { type: "engine", content: args.length === 0 ? `/${command.name}` : `/${command.name} ${args}` }
}

/** Human label for an extension command's provenance. */
export function commandSourceLabel(command: Pick<CommandChoice, "name" | "source">): string {
  switch (command.source) {
    case "project": return "project command"
    case "user": return "user command"
    case "plugin": return "plugin"
    case "skill": return "skill"
    case "workflow": return "workflow"
    case "mcp": {
      const server = /^mcp\.([^.]+)\./u.exec(command.name)?.[1]
      return server === undefined ? "mcp" : `mcp · ${server}`
    }
    default:
      return "built-in"
  }
}
