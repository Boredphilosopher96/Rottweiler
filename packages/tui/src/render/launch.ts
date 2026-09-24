import type { RottweilerState } from "../state"
import { providerName } from "../ui-presentation"
import { modelDisplayLabel } from "./format"
import { truncateToCells } from "./text"

/**
 * Compact welcome: where the session runs, which model answers, and what the
 * workspace contributes. Inventories appear only once observed and non-empty.
 */
export function launchSummary(state: RottweilerState, width: number): string {
  const line = (text: string) => truncateToCells(text.replace(/[\u0000-\u001f\u007f-\u009f]/g, " "), Math.max(12, width - 4))
  const workspace = state.workspaceStatus
  const place = workspace === null
    ? "Rottweiler"
    : `${workspace.workspaceName}${workspace.branch === null ? "" : ` · ${workspace.branch}`}`
  return [
    line(place),
    line(launchModel(state)),
    ...[launchInventory(state)].filter(text => text !== "").map(line),
    "Describe a task, or press / for commands.",
  ].join("\n")
}

function launchModel(state: RottweilerState): string {
  const name = modelDisplayLabel(state.model, state.models)
  if (name !== null) {
    const provider = state.models.find(model => model.id === state.model || model.aliases.includes(state.model ?? ""))?.provider
      ?? (state.model?.includes("/") === true ? state.model.slice(0, state.model.indexOf("/")) : null)
    return provider === null ? name : `${name} · ${providerName(provider)}`
  }
  if (!state.modelCatalogLoaded || state.modelCatalogCached) return "Loading models…"
  return state.providers.some(provider => provider.configured && provider.authenticated)
    ? "Choose a model with /model"
    : "Connect a provider with /model to start"
}

function launchInventory(state: RottweilerState): string {
  const instructions = state.context?.items.filter(item =>
    item.kind === "project_instructions" && !item.state.evicted && !item.state.pruned) ?? []
  const skills = state.commandCatalogLoaded ? state.commands.filter(command => command.source === "skill").length : 0
  const mcp = state.mcpCatalogLoaded ? state.mcpServers.filter(server => server.state.type === "ready").length : 0
  const plural = (count: number, noun: string) => `${count}${state.commandsTruncated && noun === "skill" ? "+" : ""} ${noun}${count === 1 ? "" : "s"}`
  return [
    instructions.length === 0 ? "" : instructions.length === 1
      ? `${instructions[0]!.label} loaded`
      : `${instructions.length} instruction files loaded`,
    skills === 0 ? "" : plural(skills, "skill"),
    mcp === 0 ? "" : plural(mcp, "MCP server"),
  ].filter(Boolean).join(" · ")
}
