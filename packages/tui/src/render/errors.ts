export type ErrorSeverity = "info" | "warning" | "error"

export interface PresentErrorInput {
  readonly category?: string
  readonly code?: string
  readonly message?: string
  readonly requestId?: string
}

export interface PresentedError {
  readonly text: string
  readonly severity: ErrorSeverity
}

type KnownError = Omit<PresentedError, "text"> & { readonly text: string }

const KNOWN_ERRORS: Record<string, KnownError> = {
  "internal:agent_loop": error("The engine could not complete this response"),
  "internal:substituted": error("The engine could not complete this response"),
  "protocol:attachment_unavailable": warning("Attachments are unavailable right now · try again"),
  "protocol:attachment_queue_unsupported": error("Attachments can only be sent when the session is idle"),
  "protocol:command_attachments_unsupported": error("Slash commands do not accept attachments"),
  "protocol:command_catalog_truncated": warning("The command catalog is too large to display safely"),
  "protocol:command_not_available": error("This action is not available in this engine version"),
  "protocol:command_serialization": error("Rottweiler could not prepare that request"),
  "protocol:diff_unavailable": error("No retained session diff is available"),
  "protocol:driver_lease_held": error("Another client controls this session"),
  "protocol:driver_required": error("Take control of this session before trying again"),
  "protocol:fork_attach_failed": warning("The session was forked, but Rottweiler could not open the child session"),
  "protocol:historical_replay_read_only": error("Historical replay is read-only"),
  "protocol:invalid_attachment": error("One or more attachments could not be used"),
  "protocol:invalid_command": error("The engine could not understand that request"),
  "protocol:invalid_command_arguments": error("Those command arguments are not valid"),
  "protocol:interrupt_unavailable": warning("The engine connection is unavailable · retry shortly"),
  "protocol:mcp_endpoint_invalid": error("MCP endpoints must be safe HTTPS URLs"),
  "protocol:mcp_name_invalid": error("The MCP server name is not valid"),
  "protocol:mcp_unconfigured": error("No MCP servers are configured"),
  "protocol:model_switch_pending": warning("Finish the pending model switch before choosing another model"),
  "protocol:model_unavailable": error("The selected model is unavailable"),
  "protocol:models_unavailable": error("No configured model routes are available"),
  "protocol:permission_projection_failed": warning("Could not load permission rules"),
  "protocol:permissions_projection_failed": warning("Could not load permission rules"),
  "protocol:provider_activation_failed": warning("Provider activation failed · try again"),
  "protocol:provider_activation_pending": info("Credential stored securely · activation is pending"),
  "protocol:provider_auth_failed": error("Sign-in could not be completed · try again"),
  "protocol:provider_auth_unavailable": error("This provider has no safe sign-in action"),
  "protocol:provider_credential_failed": error("The credential could not be saved · verify it and try again"),
  "protocol:provider_credential_warning": warning("The provider reported a credential warning"),
  "protocol:providers_unavailable": error("No configured provider routes are available"),
  "protocol:question_attachments_unsupported": error("Questions can only be answered with text"),
  "protocol:request_id_conflict": error("This request identifier was already used for another action"),
  "protocol:request_state_invalid": error("The engine could not process that request"),
  "protocol:review_command_failed": warning("The review decision could not be delivered · try again"),
  "protocol:review_command_unavailable": warning("The engine did not acknowledge the review decision"),
  "protocol:review_unavailable_during_shell": error("Exit the foreground shell before opening session review"),
  "protocol:selection_copy_failed": error("Couldn't copy the selected text to the clipboard"),
  "protocol:session_mismatch": error("This request belongs to a different session"),
  "protocol:session_requires_recovery": warning("Restoring this session · input will be available shortly"),
  "internal:session_requires_recovery": warning("Restoring this session · input will be available shortly"),
  "protocol:subagent_attachments_unsupported": error("Child follow-ups can only include text"),
  "protocol:subagent_close_failed": warning("Couldn't close the child agent · try again"),
  "protocol:subagent_close_unavailable": warning("The engine connection is unavailable · retry shortly"),
  "protocol:subagent_continue_failed": warning("Couldn't continue the child agent · try again"),
  "protocol:subagent_continue_unavailable": warning("The engine connection is unavailable · retry shortly"),
  "protocol:subagent_interrupt_failed": warning("Couldn't interrupt the child agent · try again"),
  "protocol:subagent_interrupt_unavailable": warning("The engine connection is unavailable · retry shortly"),
  "protocol:subagent_replay_failed": warning("Couldn't load the child transcript · try again"),
  "protocol:subagent_replay_unavailable": warning("The engine connection is unavailable · retry shortly"),
  "protocol:subagent_still_running": error("This child is still working. Inspect its progress or interrupt it before sending a follow-up."),
  "protocol:subagents_failed": warning("Couldn't load child agents · try again"),
  "protocol:subagents_unavailable": warning("The engine connection is unavailable · retry shortly"),
  "protocol:subagents_unavailable_in_replay": error("Child-agent controls are unavailable in historical replay"),
  "protocol:theme_persistence_failed": warning("The theme could not be saved · try again"),
  "protocol:tool_approval_failed": warning("Couldn't deliver the approval decision · try again"),
  "protocol:tool_approval_unavailable": warning("The engine did not acknowledge the approval decision"),
  "protocol:unsupported_protocol_version": error("Rottweiler and engine versions are incompatible"),
  "protocol:user_shell_active": warning("Finish the foreground shell before starting an agent turn"),
}

function info(text: string): KnownError {
  return { text, severity: "info" }
}

function warning(text: string): KnownError {
  return { text, severity: "warning" }
}

function error(text: string): KnownError {
  return { text, severity: "error" }
}

/** Converts engine and transport failures into stable, safe UI copy. */
export function presentError(input: PresentErrorInput): PresentedError {
  const category = input.category ?? ""
  const code = input.code ?? ""
  if (category === "protocol" && (code === "subagent_replay_gap" || code.endsWith("_projection_failed"))) {
    return {
      text: sanitizeErrorFragment(input.message),
      severity: inferredSeverity(category, code, input.message ?? ""),
    }
  }
  const known = KNOWN_ERRORS[`${category}:${code}`] ?? dynamicKnownError(category, code)
  const requestSuffix = presentRequestId(input.requestId)
  if (known !== undefined) {
    return {
      text: `${known.text}${requestSuffix}`,
      severity: known.severity,
    }
  }

  const fragment = sanitizeErrorFragment(input.message)
  return {
    text: `Something went wrong · ${fragment}${requestSuffix}`,
    severity: inferredSeverity(category, code, input.message ?? ""),
  }
}

function dynamicKnownError(category: string, code: string): KnownError | undefined {
  if (category !== "protocol") return undefined
  if (code.endsWith("_unavailable")) return warning("The engine connection is unavailable · retry shortly")
  if (code.endsWith("_failed")) return warning("That action could not be completed · try again")
  return undefined
}

function presentRequestId(requestId: string | undefined): string {
  if (requestId === undefined) return ""
  const value = truncateToCells(
    requestId.replace(/[\u0000-\u001f\u007f-\u009f]/g, "").replace(/\s+/g, " ").trim(),
    64,
  )
  return value.length === 0 ? "" : ` · request ${value}`
}

/**
 * Produces a bounded, single-line error fragment safe to embed in TUI-authored copy.
 * Control characters and trailing stack/path frames are removed; no severity or framing is added.
 */
export function sanitizeErrorFragment(message: string | undefined): string {
  const source = message ?? ""
  const stackStart = source.search(/\s+at\s/)
  const pathStart = source.match(/\S+\.(?:ts|rs):\d/)?.index ?? -1
  const fragmentStart = [stackStart, pathStart].filter((index) => index >= 0).sort((left, right) => left - right)[0] ?? -1
  const withoutStack = fragmentStart === -1 ? source : source.slice(0, fragmentStart)
  const collapsed = withoutStack
    .replace(/[\u0000-\u001f\u007f-\u009f]/g, " ")
    .replace(/\s+/g, " ")
    .trim()
  return truncateToCells(collapsed.length === 0 ? "details unavailable" : collapsed, 160)
}

function inferredSeverity(category: string, code: string, message: string): ErrorSeverity {
  const detail = `${code} ${message}`.toLowerCase()
  if (/pending|stored/.test(detail)) return "info"
  if (/connection|retry|transient|unavailable|offline|reconnect|recovery|replay|busy/.test(detail)) {
    return "warning"
  }
  if (category === "internal" || /permission|validation|invalid|denied|forbidden|auth|credential|unsupported/.test(detail)) {
    return "error"
  }
  return "error"
}
import { truncateToCells } from "./text"
