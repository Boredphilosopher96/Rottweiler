import type { RottweilerState } from "./state"

export type CommandChoice = RottweilerState["commands"][number]

export const TUI_SLASH_COMMANDS: readonly CommandChoice[] = [
  { name: "models", description: "Switch the active model", usage: "/models" },
  { name: "providers", description: "Choose a configured provider and model", usage: "/providers" },
  { name: "agents", description: "Inspect and manage child agents", usage: "/agents" },
  { name: "theme", description: "Preview and change the interface theme", usage: "/theme" },
  { name: "settings", description: "Change safe user settings", usage: "/settings" },
  { name: "exit", description: "Close Rottweiler", usage: "/exit" },
]

const TUI_HANDLED_SLASH_COMMAND_NAMES = new Set([
  ...TUI_SLASH_COMMANDS.map((command) => command.name),
  "fork",
  "mcp",
  "permissions",
  "review",
  "rewind",
])

export function isTuiHandledSlashCommand(name: string): boolean {
  return TUI_HANDLED_SLASH_COMMAND_NAMES.has(name)
}

/** Engine descriptors augment the small set of commands owned by the TUI. */
export function mergeSlashCommandChoices(
  liveCommands: readonly CommandChoice[],
): readonly CommandChoice[] {
  const choices = new Map(TUI_SLASH_COMMANDS.map((command) => [command.name, command]))
  for (const command of liveCommands) choices.set(command.name, command)
  return [...choices.values()]
}

export type SessionAction =
  | { readonly type: "exit" }
  | { readonly type: "review" }
  | { readonly type: "fork"; readonly atTurn: string | null }
  | { readonly type: "rewindTimeline" }
  | { readonly type: "models" }
  | { readonly type: "providers" }
  | { readonly type: "agents" }
  | { readonly type: "theme" }
  | { readonly type: "settings" }
  | { readonly type: "permissions" }
  | { readonly type: "mcp" }
  | { readonly type: "invalid"; readonly message: string }

export function parseSessionAction(content: string): SessionAction | null {
  const tokens = content.trim().split(/\s+/)
  const command = tokens[0]
  if (command === "/exit") {
    return tokens.length === 1
      ? { type: "exit" }
      : { type: "invalid", message: `usage: ${command}` }
  }
  if (command === "/review") {
    return tokens.length === 1
      ? { type: "review" }
      : { type: "invalid", message: "usage: /review" }
  }
  if (command === "/rewind" && tokens.length === 1) return { type: "rewindTimeline" }
  if (command === "/models") {
    return tokens.length === 1
      ? { type: "models" }
      : { type: "invalid", message: "usage: /models" }
  }
  if (command === "/providers") {
    return tokens.length === 1
      ? { type: "providers" }
      : { type: "invalid", message: "usage: /providers" }
  }
  if (command === "/agents") {
    return tokens.length === 1
      ? { type: "agents" }
      : { type: "invalid", message: "usage: /agents" }
  }
  if (command === "/theme") {
    return tokens.length === 1
      ? { type: "theme" }
      : { type: "invalid", message: "usage: /theme" }
  }
  if (command === "/settings") {
    return tokens.length === 1
      ? { type: "settings" }
      : { type: "invalid", message: "usage: /settings" }
  }
  if (command === "/permissions") return tokens.length === 1 ? { type: "permissions" } : null
  if (command === "/mcp") return tokens.length === 1 ? { type: "mcp" } : null
  if (command !== "/fork") return null
  if (tokens.length === 1) return { type: "fork", atTurn: null }
  if (tokens.length !== 2 || !isU64(tokens[1] ?? "")) {
    return { type: "invalid", message: "usage: /fork [turn] where turn is a decimal u64" }
  }
  return { type: "fork", atTurn: tokens[1] ?? null }
}

export function commandSourceLabel(source: CommandChoice["source"]): string {
  switch (source) {
    case "project": return "Project"
    case "user": return "User"
    case "plugin": return "Plugin"
    case "skill": return "Skills"
    case "workflow": return "Workflows"
    case "mcp": return "MCP"
    case "builtin":
    case undefined:
      return "Built-in"
  }
}

export function isU64(value: string): boolean {
  return /^(0|[1-9][0-9]*)$/.test(value) && BigInt(value) <= 18_446_744_073_709_551_615n
}
