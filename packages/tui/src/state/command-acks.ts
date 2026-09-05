import {
  type RottweilerState
} from "./model"

export const MAX_COMMAND_ACKS = 256

export function responseAck(
  state: RottweilerState,
  requestId: string,
  responseType:
    | "context_snapshot_ready"
    | "cost_snapshot_ready"
    | "session_review_ready"
    | "session_review_updated"
    | "prompt_dump_ready"
    | "session_replay_completed"
    | "session_forked"
    | "session_exported"
    | "sessions_listed"
    | "subagents_listed"
    | "subagent_replay_batch"
    | "subagent_replay_completed"
    | "sessions_search_ready"
    | "command_descriptors_listed"
    | "modes_listed"
    | "models_listed"
    | "settings_listed"
    | "mcp_servers_listed"
    | "runtime_services_listed"
    | "mcp_server_approval_reviewed"
    | "permissions_listed"
    | "provider_auth_started"
    | "provider_configured"
    | "provider_auth_finished"
    | "provider_activation_finished"
    | "workspace_files_found"
    | "workspace_file_preview_ready"
    | "workspace_status_ready"
    | "workspace_diff_ready"
    | "host_shutdown",
  sessionId: string | null,
): RottweilerState["commandAcks"] {
  return boundedCommandAcks(state.commandAcks, requestId, {
    requestId,
    responseType,
    outcome: null,
    sessionId,
  })
}

export function boundedCommandAcks(
  current: RottweilerState["commandAcks"],
  requestId: string,
  acknowledgement: RottweilerState["commandAcks"][string],
): RottweilerState["commandAcks"] {
  const next = { ...current }
  delete (next as Record<string, unknown>)[requestId]
    ; (next as Record<string, unknown>)[requestId] = acknowledgement
  const overflow = Object.keys(next).length - MAX_COMMAND_ACKS
  for (const key of Object.keys(next).slice(0, Math.max(0, overflow))) {
    delete (next as Record<string, unknown>)[key]
  }
  return next
}
