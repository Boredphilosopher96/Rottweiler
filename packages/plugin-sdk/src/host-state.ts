import { RPC_METHODS, type JsonValue, type PluginPushMethod } from "./generated/protocol-3"
import type {
  ExtensionSessionSnapshot,
  ExtensionStateCommitOutcome,
  ExtensionStateSnapshot,
  ExtensionStateTransaction,
} from "./generated/extension-contract"
import validateSession from "./generated/extension-session-snapshot-validator.js"
import validateState from "./generated/extension-state-snapshot-validator.js"
import validateOutcome from "./generated/extension-state-outcome-validator.js"
import validateTransaction from "./generated/extension-state-transaction-validator.js"

/** Namespace and session identities are selected by the attached host. */
export interface HostSessionApi {
  query(): Promise<ExtensionSessionSnapshot>
}

/** Delivery acknowledgement is supplied only by the host's event worker. */
export type ExtensionStateWrite = Omit<ExtensionStateTransaction, "acknowledged">

export interface HostStateApi {
  read(): Promise<ExtensionStateSnapshot>
  /** A conflict is an outcome; callers choose whether to read and retry. */
  commit(transaction: ExtensionStateWrite): Promise<ExtensionStateCommitOutcome>
}

type HostRequest = (method: PluginPushMethod, params: JsonValue) => Promise<JsonValue>

/** Uses the ordinary bounded, correlated host-request path and its admission. */
export function hostStateContext(request: HostRequest): {
  readonly session: HostSessionApi
  readonly state: HostStateApi
} {
  return {
    session: { query: async () => {
      const result = await request(RPC_METHODS.sessionQuery, {})
      if (!validateSession(result)) throw new Error("invalid host session snapshot")
      return result
    } },
    state: {
      read: async () => {
        const result = await request(RPC_METHODS.stateRead, {})
        if (!validateState(result)) throw new Error("invalid host extension state snapshot")
        return result
      },
      commit: async transaction => {
        if (Object.hasOwn(transaction, "acknowledged")) {
          throw new Error("delivery acknowledgement is host-owned")
        }
        const params = { ...transaction, acknowledged: null }
        if (!validateTransaction(params)) throw new Error("invalid extension state transaction")
        const result = await request(RPC_METHODS.stateCommit, params)
        if (!validateOutcome(result)) throw new Error("invalid host extension state outcome")
        return result
      },
    },
  }
}
