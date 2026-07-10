import type { WireEngineEvent } from "../transport"

export type RottweilerAction =
  | { readonly type: "engine_event"; readonly event: WireEngineEvent }
  | { readonly type: "transport_connecting"; readonly attempt: number }
  | { readonly type: "transport_connected"; readonly attempt: number }
  | {
      readonly type: "transport_disconnected"
      readonly attempt: number
      readonly error?: string
    }
  | { readonly type: "transport_closed" }

export const engineEvent = (event: WireEngineEvent): RottweilerAction => ({
  type: "engine_event",
  event,
})

export const transportConnecting = (attempt: number): RottweilerAction => ({
  type: "transport_connecting",
  attempt,
})

export const transportConnected = (attempt: number): RottweilerAction => ({
  type: "transport_connected",
  attempt,
})

export const transportDisconnected = (
  attempt: number,
  error?: string,
): RottweilerAction => ({
  type: "transport_disconnected",
  attempt,
  ...(error === undefined ? {} : { error }),
})

export const transportClosed = (): RottweilerAction => ({ type: "transport_closed" })
