import type { RottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type ClientCommand, type CommandOutcome } from "../../src/protocol"

/** Return an authoritative, correlated readiness projection for navigation fixtures. */
export function readyCatalog(getApp: () => RottweilerApp): (command: ClientCommand) => CommandOutcome {
  return (command) => {
    if (command.type === "list_commands") queueMicrotask(() => {
      const app = getApp()
      app.handleEvent({
        type: "command_descriptors_listed",
        meta: { ...command.meta, protocol_version: PROTOCOL_VERSION, emitted_at: "2026-09-15T00:00:00Z" },
        session_id: command.session_id,
        commands: app.state.commands.map(command => ({ ...command, source: command.source ?? "builtin", scope: command.scope ?? null })), truncated: false,
        available_actions: (["compact", "switch_model", "switch_mode", "rewind", "review", "fork", "add_workspace_root", "mutate_context"] as const).map(action => ({
          action, unavailable_reason: null,
        })),
      })
    })
    return { type: "accepted" }
  }
}
