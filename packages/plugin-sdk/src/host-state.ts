import { RPC_METHODS, type JsonValue, type PluginPushMethod } from "./generated/protocol-3"
import type {
  ExtensionSessionSnapshot, ExtensionContextRead, ExtensionContextPage, ExtensionControl, ExtensionControlOutcome,
  ExtensionStateCommitOutcome,
  ExtensionStateSnapshot,
  ExtensionStateTransaction,
} from "./generated/extension-contract"
import validateContextRead from "./generated/extension-context-read-validator.js"
import validateContextPage from "./generated/extension-context-page-validator.js"
import validateControl from "./generated/extension-control-validator.js"
import validateControlOutcome from "./generated/extension-control-outcome-validator.js"
import validateSession from "./generated/extension-session-snapshot-validator.js"
import validateState from "./generated/extension-state-snapshot-validator.js"
import validateOutcome from "./generated/extension-state-outcome-validator.js"
import validateTransaction from "./generated/extension-state-transaction-validator.js"

/** Namespace and session identities are selected by the attached host. */
export interface HostSessionApi {
  query(): Promise<ExtensionSessionSnapshot>
  readContext(request: ExtensionContextRead): Promise<ExtensionContextPage>
  control(operation: ExtensionControl): Promise<ExtensionControlOutcome>
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
    session: {
      readContext: async params => {
        if (!validateContextRead(params)) throw new Error("invalid context read")
        const result = await request(RPC_METHODS.contextRead, params)
        if (!validateContextPage(result)) throw new Error("invalid context page")
        return result
      },
      control: async params => {
        if (!validateControl(params)) throw new Error("invalid session control")
        const result = await request(RPC_METHODS.sessionControl, params)
        if (!validateControlOutcome(result)) throw new Error("invalid control outcome")
        return result
      },
      query: async () => {
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
