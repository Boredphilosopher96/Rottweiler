import type { RottweilerState } from "./state"

export type CommandChoice = RottweilerState["commands"][number]

export const LOCAL_SLASH_COMMANDS: readonly CommandChoice[] = [
  { name: "help", description: "List available commands", usage: "/help" },
  { name: "status", description: "Show actor running and queue state", usage: "/status" },
  { name: "mode", description: "Show or switch the interaction mode", usage: "/mode [id]" },
  { name: "models", description: "Switch the active model", usage: "/models" },
  { name: "providers", description: "Choose a configured provider and model", usage: "/providers" },
  { name: "agents", description: "Inspect and manage child agents", usage: "/agents" },
  { name: "theme", description: "Preview and change the interface theme", usage: "/theme" },
  { name: "settings", description: "Change safe user settings", usage: "/settings" },
  { name: "permissions", description: "Show or edit session permission rules", usage: "/permissions [list|mode|approvals|add|remove|clear-session|revoke-session|revoke-project]" },
  { name: "plan", description: "Show the pending or approved plan", usage: "/plan" },
  { name: "rewind", description: "Restore a completed turn checkpoint", usage: "/rewind <turn>" },
  { name: "fork", description: "Fork this session at a completed turn", usage: "/fork [turn]" },
  { name: "review", description: "Review the cumulative session diff", usage: "/review" },
  { name: "interrupt", description: "Interrupt the active turn", usage: "/interrupt" },
  { name: "context", description: "Inspect, pin, or evict context items", usage: "/context [pin|evict <item-id>]" },
  { name: "cost", description: "Show usage, cost, and budget accounting", usage: "/cost" },
  { name: "compact", description: "Compact conversation context", usage: "/compact [instructions]" },
  { name: "trust", description: "Inspect or change folder trust", usage: "/trust [status|grant|revoke]" },
  { name: "add-dir", description: "Append a live workspace root", usage: "/add-dir <path>" },
  { name: "exit", description: "Close Rottweiler", usage: "/exit" },
]

const LOCAL_SLASH_COMMAND_NAMES = new Set(LOCAL_SLASH_COMMANDS.map((command) => command.name))

export function isLocalSlashCommand(name: string): boolean {
  return LOCAL_SLASH_COMMAND_NAMES.has(name)
}

/** Live descriptors replace local copy while preserving stable local-first ordering. */
export function mergeSlashCommandChoices(
  liveCommands: readonly CommandChoice[],
): readonly CommandChoice[] {
  const choices = new Map(LOCAL_SLASH_COMMANDS.map((command) => [command.name, command]))
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
