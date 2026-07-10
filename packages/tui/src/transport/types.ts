import type { ClientCommand, EngineEvent } from "../protocol"

export interface UnknownEngineEvent {
  readonly type: string
  readonly meta?: unknown
  readonly [key: string]: unknown
}

export type WireEngineEvent = EngineEvent | UnknownEngineEvent

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

export function durableSequenceId(event: WireEngineEvent): string | null {
  if (!isRecord(event.meta)) {
    return null
  }
  return typeof event.meta.sequence_id === "string" ? event.meta.sequence_id : null
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}
