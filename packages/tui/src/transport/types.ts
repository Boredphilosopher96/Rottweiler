import type { ClientCommand, EngineEvent } from "../protocol"

export interface UnknownEngineEvent {
  readonly type: string
  readonly meta?: unknown
  readonly [key: string]: unknown
}

export type WireEngineEvent = EngineEvent | UnknownEngineEvent
export type SessionForkedEvent = Extract<EngineEvent, { type: "session_forked" }>

export type AttachSessionCommand = Extract<ClientCommand, { type: "attach_session" }>

export type TransportConnectionPhase =
  | "connecting"
  | "connected"
  | "reconnecting"
  | "disconnected"
  | "closed"

export interface TransportConnectionUpdate {
  readonly phase: TransportConnectionPhase
  readonly attempt: number
  readonly error?: string
}

export function isWireEngineEvent(value: unknown): value is WireEngineEvent {
  return isRecord(value) && typeof value.type === "string"
}

export function normalizeWireEngineEvent(value: unknown): WireEngineEvent | null {
  return isWireEngineEvent(value) ? value : null
}

export function isSessionForkedEvent(event: WireEngineEvent): event is SessionForkedEvent {
  if (event.type !== "session_forked" || !isRecord(event.child)) return false
  const child = event.child
  return (
    typeof event.parent_session_id === "string" &&
    typeof child.session_id === "string" &&
    typeof child.workspace_name === "string" &&
    typeof child.model === "string" &&
    (child.driver_client_id === undefined ||
      child.driver_client_id === null ||
      typeof child.driver_client_id === "string") &&
    typeof child.shell_active === "boolean" &&
    (event.at_turn === undefined || event.at_turn === null || typeof event.at_turn === "string")
  )
}

export function durableSequenceId(event: WireEngineEvent): string | null {
  if (!("meta" in event) || !isRecord(event.meta)) {
    return null
  }
  return typeof event.meta.sequence_id === "string" ? event.meta.sequence_id : null
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}
