import type { RottweilerState } from "../state"
import { truncateToCells } from "./text"

/** Describe observed projections; missing inventories are not empty inventories. */
export function launchSummary(state: RottweilerState, width: number): string {
  const line = (text: string) => truncateToCells(text.replace(/[\u0000-\u001f\u007f-\u009f]/g, " "), Math.max(12, width - 4))
  const workspace = state.workspaceStatus
  const instructions = state.context?.items.filter(item => item.kind === "project_instructions" && !item.state.evicted && !item.state.pruned)
  const skills = state.commands.filter(command => command.source === "skill")
  const connected = state.mcpServers.filter(server => server.state.type === "ready").length
  const catalogQualifier = state.commandsTruncated ? "+ · catalog truncated" : ""
  const resolved = state.models.find(model => (model.id === state.model || (state.model !== null && model.aliases.includes(state.model))) && model.available !== false)
  const model = resolved === undefined ? "Choose a model · /models · /providers to connect" : `${resolved.displayName} · ${resolved.provider}`
  return [
    line(workspace === null ? "Workspace · not loaded" : `Workspace · ${workspace.workspaceName} · ${workspace.branch ?? "no Git branch"}`),
    line(instructions === undefined ? "Instructions · not loaded" : instructions.length === 0 ? "Instructions · none active" : `Instructions · ${instructions.map(item => `${item.label} (${item.source})`).join(", ")}`),
    line(!state.mcpCatalogLoaded ? "MCP · not loaded" : `MCP · ${connected} ready / ${state.mcpServers.length} configured`),
    line(!state.commandCatalogLoaded ? "Skill commands · not loaded" : `Skill commands · ${skills.length}${catalogQualifier}${skills.length === 0 ? "" : ` · ${skills.map(skill => `/${skill.name}`).join(", ")}`}`),
    line(`Model · ${model}`),
    "Describe a task, or press / for commands.",
  ].join("\n")
}
