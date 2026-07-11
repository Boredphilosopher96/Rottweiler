import type {
  ClientCommand as GeneratedClientCommand,
  EngineEvent as GeneratedEngineEvent,
} from "../../../protocol/types"

export { PROTOCOL_VERSION } from "../../../protocol/types"
export type {
  Answer,
  Attachment,
  AttachmentData,
  ApprovalBinding,
  ApprovalDecision,
  BudgetLevel,
  BudgetScope,
  BudgetUnit,
  ClientRole,
  CommandOutcome,
  ContextSnapshot,
  Cost,
  CostSnapshot,
  EngineError,
  PromptDump,
  Question,
  ToolCapability,
  ToolOutput,
  ToolOutputStream,
  Turn,
  TurnStatus,
  Usage,
  UnifiedDiff,
} from "../../../protocol/types"

/**
 * The TUI only consumes protocol types generated from the Rust source of truth.
 * Keeping this boundary in one module makes that ownership explicit and gives
 * the future transport client a stable local import.
 */
export type ClientCommand = GeneratedClientCommand
export type EngineEvent = GeneratedEngineEvent
