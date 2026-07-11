export const PLUGIN_PROTOCOL_VERSION = 1 as const

export const RPC_METHODS = Object.freeze({
  initialize: "initialize",
  toolCall: "tool/call",
  commandExecute: "command/execute",
  hookInvoke: "hook/invoke",
  providerComplete: "provider/complete",
  providerEvent: "provider/event",
  providerCancel: "provider/cancel",
  eventPublish: "event/publish",
  injectMessage: "session/inject_message",
  setStatus: "session/set_status",
  notify: "ui/notify",
  shutdown: "shutdown",
  exit: "exit",
} as const)

export const PROTOCOL_LIMITS = {
  maxLineBytes: 4 * 1024 * 1024,
  maxManifestBytes: 256 * 1024,
  maxCapabilitiesPerKind: 256,
  maxNameBytes: 128,
  maxVersionBytes: 64,
  maxDescriptionBytes: 16 * 1024,
  maxSchemaBytes: 64 * 1024,
  maxSchemaDepth: 32,
  maxRpcMessageBytes: 16 * 1024,
  maxHookPayloadBytes: 256 * 1024,
  defaultHandlerTimeoutMs: 5_000,
} as const

export type JsonPrimitive = boolean | number | string | null
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue }
export type JsonObject = { [key: string]: JsonValue }
export type JsonSchema = JsonObject

export type ToolCapability = "reads-fs" | "writes-fs" | "network" | "exec"

export interface ToolDeclaration {
  readonly name: string
  readonly description: string
  readonly schema: JsonSchema
  readonly caps: readonly ToolCapability[]
}

export interface CommandDeclaration {
  readonly name: string
  readonly description: string
  readonly argument_hint?: string
  readonly allowed_tools?: readonly string[]
}

export type HookName =
  | "session_start"
  | "session_end"
  | "user_prompt_submit"
  | "pre_tool"
  | "post_tool"
  | "pre_compact"
  | "turn_end"
  | "permission_check"

export type HookFailurePolicy = "fail-open" | "fail-closed"

export interface HookDeclaration {
  readonly name: HookName
  readonly failure_policy: HookFailurePolicy
}

export interface ProviderDeclaration {
  readonly "alias-prefix": string
}

export type PluginPushMethod =
  | "session/inject_message"
  | "session/set_status"
  | "ui/notify"

export interface PluginCapabilities {
  readonly tools?: readonly ToolDeclaration[]
  readonly commands?: readonly CommandDeclaration[]
  readonly hooks?: readonly (HookName | HookDeclaration)[]
  readonly providers?: readonly ProviderDeclaration[]
  readonly event_subscriptions?: readonly string[]
  readonly push?: readonly PluginPushMethod[]
}

export interface PluginManifest {
  readonly name: string
  readonly version: string
  readonly protocol: typeof PLUGIN_PROTOCOL_VERSION
  readonly capabilities: PluginCapabilities
}

export interface InitializeParams {
  readonly host: string
  readonly protocol: number
  readonly min_protocol: number
  readonly max_frame_bytes: number
}

export interface ToolCallParams {
  readonly name: string
  readonly input: JsonObject
}

/** Exact wire result consumed by rw-tools::ToolResult. */
export interface ToolResponse {
  readonly content: string
  readonly data: JsonValue
  readonly truncated?: boolean
}

export interface CommandExecuteParams {
  readonly name: string
  readonly arguments: string
}

export interface HookInvokeParams {
  readonly hook: HookName
  readonly payload: JsonObject
}

export type HookDecision =
  | { readonly decision: "allow"; readonly payload?: JsonValue }
  | { readonly decision: "deny"; readonly message: string }
  | { readonly decision: "replace"; readonly payload: JsonValue }

export type ProviderRole = "system" | "user" | "assistant" | "tool"

export interface ProviderTurn {
  readonly role: ProviderRole
  readonly blocks: readonly JsonValue[]
  readonly meta: {
    readonly created_at?: string | null
    readonly model?: string | null
    readonly synthetic?: boolean
    readonly summary?: boolean
  }
}

export interface ProviderToolDefinition {
  readonly name: string
  readonly description: string
  readonly input_schema: JsonSchema
}

export type ProviderToolChoice =
  | { readonly mode: "auto" | "required" | "none" }
  | { readonly mode: "named"; readonly name: string }

export type ProviderThinkingLevel = "off" | "low" | "medium" | "high"

export interface ProviderRequest {
  readonly model: string
  readonly turns: readonly ProviderTurn[]
  readonly tools: readonly ProviderToolDefinition[]
  readonly tool_choice: ProviderToolChoice
  readonly max_output_tokens: number
  readonly temperature: number | null
  readonly thinking: ProviderThinkingLevel
  readonly cache_hint?: {
    readonly stable_prefix_turns: number
    readonly tools_in_prefix: boolean
  } | null
}

export interface ProviderCompleteParams {
  readonly alias: string
  readonly request: ProviderRequest
}

export interface ProviderEventParams {
  readonly request_id: RpcId
  readonly event: ProviderEvent
}

export interface ProviderCancelParams {
  readonly request_id: RpcId
}

export interface ProviderUsage {
  readonly input_tokens: number
  readonly output_tokens: number
  readonly cache_read_tokens: number
  readonly cache_write_tokens: number
  readonly reasoning_tokens: number
}

export type ProviderFinishReason = "stop" | "length" | "tool_calls" | "content_filter" | "unknown"

export type ProviderEvent =
  | { readonly type: "route_selected"; readonly route: string }
  | { readonly type: "message_start"; readonly model: string }
  | { readonly type: "text_delta"; readonly text: string }
  | { readonly type: "thinking_delta"; readonly content: string; readonly signature: string | null }
  | { readonly type: "tool_call_start"; readonly id: string; readonly name: string }
  | { readonly type: "tool_call_arguments_delta"; readonly id: string; readonly json_fragment: string }
  | { readonly type: "tool_call_end"; readonly id: string; readonly arguments: JsonValue }
  | { readonly type: "citation"; readonly uri: string; readonly title: string | null; readonly start_index: number | null; readonly end_index: number | null }
  | { readonly type: "usage"; readonly usage: ProviderUsage }
  | { readonly type: "finished"; readonly reason: ProviderFinishReason }

export type ProviderStream = AsyncIterable<ProviderEvent>

export interface EventPublishParams {
  readonly event: string
  readonly payload: JsonObject
}

export type RpcId = number | string

export interface RpcRequest {
  readonly jsonrpc: "2.0"
  readonly id: RpcId
  readonly method: string
  readonly params?: JsonValue
}

export interface RpcNotification {
  readonly jsonrpc: "2.0"
  readonly method: string
  readonly params?: JsonValue
}

export interface RpcErrorObject {
  readonly code: number
  readonly message: string
  readonly data?: JsonValue
}

export type RpcResponse =
  | { readonly jsonrpc: "2.0"; readonly id: RpcId; readonly result: JsonValue }
  | { readonly jsonrpc: "2.0"; readonly id: RpcId | null; readonly error: RpcErrorObject }
