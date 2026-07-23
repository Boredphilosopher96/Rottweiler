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
  CommandSource,
  ContextSnapshot,
  Cost,
  CostSnapshot,
  EngineError,
  McpApprovalReview,
  McpServerDescriptor,
  McpServerState,
  ModeDescriptor,
  PromptDump,
  PlanArtifact,
  PlanDecision,
  PlanStep,
  PermissionAction,
  PermissionApprovalDescriptor,
  PermissionApprovalScope,
  PermissionModeDescriptor,
  PermissionRuleDescriptor,
  PermissionStateDescriptor,
  RuntimeServiceDescriptor,
  RuntimeServiceKind,
  ProviderAuthKind,
  ProviderAuthAttemptId,
  ProviderAuthChallenge,
  ProviderDescriptor,
  ProviderNextAction,
  Question,
  ReviewFileDecision,
  ReviewFileStatus,
  SessionReview,
  SessionReviewFile,
  SubagentResult,
  SubagentStatus,
  ToolCapability,
  ToolOutput,
  ToolOutputStream,
  Turn,
  TurnStatus,
  UserSettingDescriptor,
  ModeId,
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
