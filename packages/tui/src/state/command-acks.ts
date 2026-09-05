import {
  type CommandAcknowledgement,
  type RottweilerState
} from "./model"

export const MAX_COMMAND_ACKS = 256

export function responseAck(
  state: RottweilerState,
  requestId: string,
  responseType: Exclude<CommandAcknowledgement["responseType"], "command_acknowledged">,
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
