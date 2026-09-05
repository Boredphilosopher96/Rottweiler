use super::AgentTurnStatus;
use super::SessionUsage;
use super::permission_mode_name;
use super::wire_turn_id;
use crate::PermissionRequest;
use rw_types::Answer;
use rw_types::BudgetLevel;
use rw_types::BudgetScope;
use rw_types::BudgetUnit;
use rw_types::ClientId;
use rw_types::CompactionReason;
use rw_types::ContextItemId;
use rw_types::Cost;
use rw_types::EngineError;
use rw_types::EngineErrorCategory;
use rw_types::EngineEvent;
use rw_types::EventMeta;
use rw_types::ModeId;
use rw_types::ModelAlias;
use rw_types::ModelContextTransfer;
use rw_types::PlanArtifact;
use rw_types::PlanDecision;
use rw_types::Question;
use rw_types::QuestionId;
use rw_types::SessionId;
use rw_types::ShellId;
use rw_types::StoredAttachment;
use rw_types::SubagentId;
use rw_types::ToolCallId;
use rw_types::ToolOutput;
use rw_types::ToolOutputStream;
use rw_types::Turn;
use rw_types::UnifiedDiff;
use rw_types::UnrestorablePath;
use rw_types::config::ThinkingLevel;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Unstamped event assembled inside the single-writer actor. The only public,
/// persisted, or streamed representation is [`EngineEvent`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum PendingEvent {
    ProviderCallAccounted {
        call: rw_types::ProviderCallIdentity,
        actuals: rw_types::ProviderCallActuals,
    },
    SessionCreated {
        driver_client_id: ClientId,
    },
    WorkspaceRootsChanged {
        generation: u64,
        effective_from_turn: u64,
        roots: Vec<rw_types::WorkspaceRootDescriptor>,
    },
    TurnStarted {
        turn: u64,
    },
    UserMessageAccepted {
        turn: u64,
        content: String,
        attachments: Vec<StoredAttachment>,
    },
    SessionTitleUpdated {
        title: String,
        usage: Option<SessionUsage>,
        cost: Option<Cost>,
    },
    ConversationTurnCommitted {
        agent_turn: u64,
        turn: Turn,
    },
    ConversationRewound {
        to_turn: u64,
        operation_id: String,
        unrestorable_paths: Vec<UnrestorablePath>,
    },
    MessageQueued {
        position: u64,
        content: String,
        attachments: Vec<StoredAttachment>,
    },
    QueuedMessageRemoved {
        position: u64,
    },
    QueuedMessagesCleared,
    PluginMessageInjected {
        plugin_id: String,
        content: String,
        queued: bool,
    },
    PluginStatusChanged {
        plugin_id: String,
        status: String,
    },
    UiNotification {
        plugin_id: String,
        title: String,
        message: String,
    },
    TextDelta {
        turn: u64,
        text: String,
    },
    ThinkingDelta {
        turn: u64,
        content: String,
        signature: Option<String>,
    },
    CitationDelta {
        turn: u64,
        uri: String,
        title: Option<String>,
    },
    ToolCallStarted {
        turn: u64,
        id: String,
        invocation_id: rw_types::ToolInvocationId,
        name: String,
        arguments: Value,
        index: usize,
    },
    PermissionRequested {
        turn: u64,
        request: PermissionRequest,
    },
    ToolDiffReady {
        turn: u64,
        id: String,
        invocation_id: rw_types::ToolInvocationId,
        diff: UnifiedDiff,
    },
    ToolOutput {
        turn: u64,
        id: String,
        invocation_id: rw_types::ToolInvocationId,
        stream: String,
        chunk: String,
    },
    ToolCallFinished {
        turn: u64,
        id: String,
        invocation_id: rw_types::ToolInvocationId,
        output: ToolOutput,
        is_error: bool,
        index: usize,
    },
    HookFailure {
        event: String,
        hook_id: String,
        fail_closed: bool,
        message: String,
    },
    CommandFinished {
        name: String,
        message: String,
        unrestorable_paths: Vec<UnrestorablePath>,
    },
    GuardTriggered {
        turn: u64,
        guard: String,
        message: String,
    },
    TurnFinished {
        turn: u64,
        status: AgentTurnStatus,
        usage: SessionUsage,
        cost: Cost,
    },
    ContextUsage {
        turn: u64,
        used_tokens: u64,
        usable_tokens: u64,
        reserved_tokens: u64,
        context_window_known: bool,
        context_window_reason: Option<String>,
        stable_prefix_hash: String,
        cache_hit_basis_points: u16,
        estimated_input_tokens: u64,
        provider_input_tokens: u64,
        correction_millionths: u64,
    },
    BudgetStatus {
        turn: u64,
        level: BudgetLevel,
        scope: BudgetScope,
        unit: BudgetUnit,
        current: u64,
        limit: u64,
    },
    CompactionStarted {
        reason: CompactionReason,
    },
    CompactionAttemptFinished {
        summary_turn: u64,
        usage: SessionUsage,
        cost: Cost,
    },
    CompactionFinished {
        summary_turn: u64,
        reclaimed_tokens: u64,
        usage: Option<SessionUsage>,
        cost: Option<Cost>,
    },
    CompactionFailed {
        summary_turn: u64,
    },
    SubagentSpawned {
        subagent_id: SubagentId,
        child_session_id: SessionId,
        task: String,
    },
    SubagentFinished {
        subagent_id: SubagentId,
        result: rw_types::SubagentResult,
    },
    ToolOutputPruned {
        tool_call_id: String,
        reclaimed_tokens: u64,
    },
    ContextItemPinned {
        item_id: ContextItemId,
        effective_after_agent_turn: u64,
    },
    ContextItemEvicted {
        item_id: ContextItemId,
        effective_after_agent_turn: u64,
    },
    Error {
        message: String,
    },
    DriverChanged {
        driver_client_id: ClientId,
    },
    ModelChanged {
        model: ModelAlias,
        provider: Option<String>,
        thinking: ThinkingLevel,
    },
    ModelContextCleared {
        strategy: ModelContextTransfer,
    },
    ModeChanged {
        mode: ModeId,
        definition_fingerprint: String,
    },
    PermissionModeChanged {
        mode: Option<rw_types::PermissionModeDescriptor>,
    },
    PlanSubmitted {
        artifact: PlanArtifact,
    },
    PlanReviewed {
        artifact: PlanArtifact,
        decision: PlanDecision,
        revisions: Option<String>,
    },
    UserShellStateChanged {
        shell_id: ShellId,
        command: String,
        active: bool,
        status: Option<i32>,
        captured_output: Option<String>,
    },
    QuestionAsked {
        turn: u64,
        question_id: QuestionId,
        questions: Vec<Question>,
    },
    QuestionAnswered {
        turn: u64,
        question_id: QuestionId,
        answers: Vec<Answer>,
    },
}

impl PendingEvent {
    pub(super) fn active_turn(&self) -> Option<u64> {
        match self {
            Self::TurnStarted { turn }
            | Self::UserMessageAccepted { turn, .. }
            | Self::TextDelta { turn, .. }
            | Self::ThinkingDelta { turn, .. }
            | Self::CitationDelta { turn, .. }
            | Self::ToolCallStarted { turn, .. }
            | Self::PermissionRequested { turn, .. }
            | Self::ToolDiffReady { turn, .. }
            | Self::ToolOutput { turn, .. }
            | Self::ToolCallFinished { turn, .. }
            | Self::GuardTriggered { turn, .. }
            | Self::TurnFinished { turn, .. }
            | Self::ContextUsage { turn, .. }
            | Self::BudgetStatus { turn, .. }
            | Self::QuestionAsked { turn, .. }
            | Self::QuestionAnswered { turn, .. } => Some(*turn),
            Self::ConversationTurnCommitted { agent_turn, .. } => Some(*agent_turn),
            _ => None,
        }
    }
}

impl PendingEvent {
    #[allow(clippy::too_many_lines)]
    pub(super) fn stamp(self, meta: EventMeta) -> EngineEvent {
        match self {
            Self::ProviderCallAccounted { call, actuals } => EngineEvent::ProviderCallAccounted {
                meta,
                call,
                actuals,
            },
            Self::SessionCreated { driver_client_id } => EngineEvent::SessionCreated {
                meta,
                driver_client_id,
            },
            Self::WorkspaceRootsChanged {
                generation,
                effective_from_turn,
                roots,
            } => EngineEvent::WorkspaceRootsChanged {
                meta,
                generation,
                effective_from_turn,
                roots,
            },
            Self::TurnStarted { turn } => EngineEvent::TurnStarted {
                meta,
                turn_id: wire_turn_id(turn),
            },
            Self::UserMessageAccepted {
                turn,
                content,
                attachments,
            } => EngineEvent::UserMessageAccepted {
                meta,
                agent_turn: turn,
                content,
                attachments,
            },
            Self::SessionTitleUpdated { title, usage, cost } => EngineEvent::SessionTitleUpdated {
                meta,
                title,
                usage: usage.map(Into::into),
                cost,
            },
            Self::ConversationTurnCommitted { agent_turn, turn } => {
                EngineEvent::ConversationTurnCommitted {
                    meta,
                    agent_turn,
                    turn,
                }
            }
            Self::ConversationRewound {
                to_turn,
                operation_id,
                unrestorable_paths,
            } => EngineEvent::ConversationRewound {
                meta,
                to_agent_turn: to_turn,
                operation_id,
                unrestorable_paths,
            },
            Self::MessageQueued {
                position,
                content,
                attachments,
            } => EngineEvent::MessageQueued {
                meta,
                position,
                content,
                attachments,
            },
            Self::QueuedMessageRemoved { position } => {
                EngineEvent::QueuedMessageRemoved { meta, position }
            }
            Self::QueuedMessagesCleared => EngineEvent::QueuedMessagesCleared { meta },
            Self::PluginMessageInjected {
                plugin_id,
                content,
                queued,
            } => EngineEvent::PluginMessageInjected {
                meta,
                plugin_id,
                content,
                queued,
            },
            Self::PluginStatusChanged { plugin_id, status } => EngineEvent::PluginStatusChanged {
                meta,
                plugin_id,
                status,
            },
            Self::UiNotification {
                plugin_id,
                title,
                message,
            } => EngineEvent::UiNotification {
                meta,
                plugin_id,
                title,
                message,
            },
            Self::TextDelta { turn, text } => EngineEvent::TextDelta {
                meta,
                turn_id: wire_turn_id(turn),
                text,
            },
            Self::ThinkingDelta {
                turn,
                content,
                signature,
            } => EngineEvent::ThinkingDelta {
                meta,
                turn_id: wire_turn_id(turn),
                text: content,
                signature,
            },
            Self::CitationDelta { turn, uri, title } => EngineEvent::CitationDelta {
                meta,
                turn_id: wire_turn_id(turn),
                uri,
                title,
            },
            Self::ToolCallStarted {
                turn,
                id,
                invocation_id,
                name,
                arguments,
                index,
            } => EngineEvent::ToolCallStarted {
                meta,
                turn_id: wire_turn_id(turn),
                tool_call_id: ToolCallId(id),
                invocation_id,
                name,
                args: arguments,
                call_index: u32::try_from(index).unwrap_or(u32::MAX),
            },
            Self::PermissionRequested { turn, request } => EngineEvent::ToolApprovalNeeded {
                meta,
                turn_id: wire_turn_id(turn),
                tool_call_id: ToolCallId(request.id),
                invocation_id: request.invocation_id,
                name: request.tool_name.clone(),
                rationale: if request.arguments.get("sandbox").and_then(Value::as_str)
                    == Some("unsandboxed")
                {
                    "UNSANDBOXED EXECUTION: this command will bypass native filesystem and network isolation"
                        .to_owned()
                } else {
                    format!("permission required for tool `{}`", request.tool_name)
                },
                args: request.arguments,
                capabilities: request.capabilities,
                diff: request.approval_diff,
            },
            Self::ToolDiffReady {
                turn,
                id,
                invocation_id,
                diff,
            } => EngineEvent::ToolDiffReady {
                meta,
                turn_id: wire_turn_id(turn),
                tool_call_id: ToolCallId(id),
                invocation_id,
                diff,
            },
            Self::ToolOutput {
                turn,
                id,
                invocation_id,
                stream,
                chunk,
            } => EngineEvent::ToolOutputDelta {
                meta,
                turn_id: wire_turn_id(turn),
                tool_call_id: ToolCallId(id),
                invocation_id,
                stream: if stream == "stderr" {
                    ToolOutputStream::Stderr
                } else {
                    ToolOutputStream::Stdout
                },
                chunk,
            },
            Self::ToolCallFinished {
                turn,
                id,
                invocation_id,
                output,
                is_error,
                index,
            } => EngineEvent::ToolCallFinished {
                meta,
                turn_id: wire_turn_id(turn),
                tool_call_id: ToolCallId(id),
                invocation_id,
                output,
                is_error,
                call_index: u32::try_from(index).unwrap_or(u32::MAX),
            },
            Self::HookFailure {
                event,
                hook_id,
                fail_closed,
                message,
            } => EngineEvent::HookFailed {
                meta,
                event,
                hook_id,
                fail_closed,
                message,
            },
            Self::CommandFinished {
                name,
                message,
                unrestorable_paths,
            } => EngineEvent::CommandFinished {
                meta,
                name,
                message,
                unrestorable_paths,
            },
            Self::GuardTriggered {
                turn,
                guard,
                message,
            } => EngineEvent::GuardTriggered {
                meta,
                turn_id: wire_turn_id(turn),
                guard,
                message,
            },
            Self::TurnFinished {
                turn,
                status,
                usage,
                cost,
            } => EngineEvent::TurnFinished {
                meta,
                turn_id: wire_turn_id(turn),
                status: status.into(),
                usage: usage.into(),
                cost,
            },
            Self::ContextUsage {
                turn,
                used_tokens,
                usable_tokens,
                reserved_tokens,
                context_window_known,
                context_window_reason,
                stable_prefix_hash,
                cache_hit_basis_points,
                estimated_input_tokens,
                provider_input_tokens,
                correction_millionths,
            } => EngineEvent::ContextUsageUpdated {
                meta,
                turn_id: wire_turn_id(turn),
                used_tokens,
                usable_tokens,
                reserved_tokens,
                context_window_known,
                context_window_reason,
                stable_prefix_hash,
                cache_hit_basis_points,
                estimated_input_tokens,
                provider_input_tokens,
                correction_millionths,
            },
            Self::BudgetStatus {
                turn,
                level,
                scope,
                unit,
                current,
                limit,
            } => EngineEvent::BudgetStatusChanged {
                meta,
                turn_id: wire_turn_id(turn),
                level,
                scope,
                unit,
                current,
                limit,
            },
            Self::CompactionStarted { reason } => EngineEvent::CompactionStarted { meta, reason },
            Self::CompactionAttemptFinished {
                summary_turn,
                usage,
                cost,
            } => EngineEvent::CompactionAttemptFinished {
                meta,
                summary_turn_id: wire_turn_id(summary_turn),
                usage: usage.into(),
                cost,
            },
            Self::CompactionFinished {
                summary_turn,
                reclaimed_tokens,
                usage,
                cost,
            } => EngineEvent::CompactionFinished {
                meta,
                summary_turn_id: wire_turn_id(summary_turn),
                reclaimed_tokens,
                usage: usage.map(Into::into),
                cost,
            },
            Self::CompactionFailed { summary_turn } => EngineEvent::CompactionFailed {
                meta,
                summary_turn_id: wire_turn_id(summary_turn),
            },
            Self::SubagentSpawned {
                subagent_id,
                child_session_id,
                task,
            } => EngineEvent::SubagentSpawned {
                meta,
                subagent_id,
                child_session_id,
                task,
            },
            Self::SubagentFinished {
                subagent_id,
                result,
            } => EngineEvent::SubagentFinished {
                meta,
                subagent_id,
                result,
            },
            Self::ToolOutputPruned {
                tool_call_id,
                reclaimed_tokens,
            } => EngineEvent::ToolOutputPruned {
                meta,
                tool_call_id: ToolCallId(tool_call_id),
                reclaimed_tokens,
            },
            Self::ContextItemPinned {
                item_id,
                effective_after_agent_turn,
            } => EngineEvent::ContextItemPinned {
                meta,
                item_id,
                effective_after_agent_turn,
            },
            Self::ContextItemEvicted {
                item_id,
                effective_after_agent_turn,
            } => EngineEvent::ContextItemEvicted {
                meta,
                item_id,
                effective_after_agent_turn,
            },
            Self::Error { message } => EngineEvent::Error {
                meta,
                error: EngineError {
                    category: EngineErrorCategory::Internal,
                    code: "agent_loop".to_owned(),
                    message,
                    retryable: false,
                    details: None,
                },
            },
            Self::DriverChanged { driver_client_id } => EngineEvent::DriverChanged {
                meta,
                driver_client_id,
            },
            Self::ModelChanged {
                model,
                provider,
                thinking,
            } => EngineEvent::ModelChanged {
                meta,
                model,
                provider,
                thinking: Some(thinking),
            },
            Self::ModelContextCleared { strategy } => {
                EngineEvent::ModelContextCleared { meta, strategy }
            }
            Self::ModeChanged {
                mode,
                definition_fingerprint,
            } => EngineEvent::ModeChanged {
                meta,
                mode,
                definition_fingerprint,
            },
            Self::PermissionModeChanged { mode } => EngineEvent::PermissionModeChanged {
                meta,
                mode: mode.map(permission_mode_name).map(str::to_owned),
            },
            Self::PlanSubmitted { artifact } => EngineEvent::PlanSubmitted { meta, artifact },
            Self::PlanReviewed {
                artifact,
                decision,
                revisions,
            } => EngineEvent::PlanReviewed {
                meta,
                artifact,
                decision,
                revisions,
            },
            Self::UserShellStateChanged {
                shell_id,
                command,
                active,
                status,
                captured_output,
            } => EngineEvent::UserShellStateChanged {
                meta,
                shell_id,
                command: Some(command),
                active,
                status,
                captured_output,
            },
            Self::QuestionAsked {
                turn,
                question_id,
                questions,
            } => EngineEvent::QuestionAsked {
                meta,
                turn_id: wire_turn_id(turn),
                question_id,
                questions,
            },
            Self::QuestionAnswered {
                turn,
                question_id,
                answers,
            } => EngineEvent::QuestionAnswered {
                meta,
                turn_id: wire_turn_id(turn),
                question_id,
                answers,
            },
        }
    }
}
