use super::{
    Answer, BudgetLevel, BudgetScope, BudgetUnit, ClientId, CommandAckMeta, CommandDescriptor,
    CommandOutcome, CompactionReason, ContextItemId, ContextSnapshot, Cost, CostSnapshot,
    EngineError, EventMeta, McpApprovalReview, McpServerDescriptor, ModeDescriptor, ModeId,
    ModelAlias, ModelAliasDescriptor, ModelContextTransfer, ModelDescriptor,
    PermissionStateDescriptor, PlanArtifact, PlanDecision, PromptDump, ProviderAuthAttemptId,
    ProviderAuthChallenge, ProviderAuthKind, ProviderDescriptor, Question, QuestionId,
    ReviewFileDecision, RuntimeServiceDescriptor, SequenceId, SessionDescriptor, SessionId,
    SessionReview, ShellId, StoredAttachment, SubagentDescriptor, SubagentId, SubagentResult,
    ToolCapability, ToolOutputStream, TurnId, TurnStatus, UnifiedDiff, UnrestorablePath, Usage,
    UserSettingDescriptor, WorkspaceDiff, WorkspaceFileMatch, WorkspaceFilePreview,
    WorkspaceRootDescriptor, WorkspaceStatus, decimal_u64,
};
use crate::{ProviderCallActuals, ProviderCallIdentity, ToolCallId, ToolInvocationId, ToolOutput};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

/// Delivery lifetime owned by an engine event variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineEventDelivery {
    /// Stored in the ordered session log and replayed after reconnect.
    Durable,
    /// Returned only to the requesting connection.
    Connection,
    /// Broadcast as live progress without advancing the durable cursor.
    Transient,
}

/// Non-durable event tags that still belong to a live session stream.
pub const TRANSIENT_ENGINE_EVENT_TYPES: &[&str] = &[
    "tool_progress",
    "subagent_progress",
    "compaction_attempt_started",
    "compaction_text_delta",
    "compaction_thinking_delta",
];

/// Events streamed to clients and persisted in the session event log.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case", optional_fields = nullable)]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum EngineEvent {
    SessionNavigationRequested {
        meta: CommandAckMeta,
        session_id: SessionId,
        target: crate::extension_control::SessionNavigationTarget,
    },
    SessionStateReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        snapshot: crate::session_state::SessionStateSnapshot,
    },
    SessionControlsReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        snapshot: crate::session_controls::SessionControlsSnapshot,
    },
    FamilyControlsReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        snapshot: crate::family_controls::FamilyControlsSnapshot,
    },
    ChildControlsReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        target: crate::family_controls::ChildControlTarget,
        snapshot: crate::family_controls::ChildControlsSnapshot,
    },
    UiCatalogReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        catalog: crate::extension_ui::UiCatalog,
    },
    UiPanelsReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        panels: crate::extension_ui::UiPanels,
    },
    TodoStateCommitted {
        meta: EventMeta,
        snapshot: crate::todo::TodoSnapshot,
    },
    SessionChildrenReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        result: crate::session_children::SessionChildrenResult,
    },
    TodosRead {
        meta: CommandAckMeta,
        session_id: SessionId,
        result: crate::todo::TodoReadResult,
    },
    TranscriptTailReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        result: crate::transcript_tail::TranscriptTailResult,
    },
    TranscriptPageReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        result: crate::transcript::TranscriptReadResult,
    },
    TranscriptContentReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        page: crate::transcript::TranscriptContentPage,
    },
    CommandAcknowledged {
        meta: CommandAckMeta,
        session_id: Option<SessionId>,
        outcome: CommandOutcome,
    },
    ContextSnapshotReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        snapshot: ContextSnapshot,
    },
    CostSnapshotReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        snapshot: CostSnapshot,
    },
    SessionReviewReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        review: SessionReview,
    },
    SessionReviewUpdated {
        meta: CommandAckMeta,
        session_id: SessionId,
        path: String,
        decision: ReviewFileDecision,
        review: SessionReview,
    },
    PromptDumpReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        dump: PromptDump,
    },
    /// Paged historical content is available; no durable replay cursor is implied.
    SessionHistoryReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        through_sequence: Option<SequenceId>,
    },
    SessionReplayCompleted {
        meta: CommandAckMeta,
        session_id: SessionId,
        through_sequence: Option<SequenceId>,
    },
    SessionForked {
        meta: CommandAckMeta,
        parent_session_id: SessionId,
        child: SessionDescriptor,
        at_turn: TurnId,
    },
    SessionExported {
        meta: CommandAckMeta,
        session_id: SessionId,
        output_path: String,
    },
    SessionsListed {
        meta: CommandAckMeta,
        sessions: Vec<SessionDescriptor>,
    },
    SubagentsListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        subagents: Vec<SubagentDescriptor>,
    },
    SessionsSearchReady {
        meta: CommandAckMeta,
        query: String,
        sessions: Vec<SessionDescriptor>,
        truncated: bool,
    },
    CommandDescriptorsListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        commands: Vec<CommandDescriptor>,
        truncated: bool,
    },
    ModesListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        modes: Vec<ModeDescriptor>,
        truncated: bool,
    },
    ModelsListed {
        meta: CommandAckMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session_id: Option<SessionId>,
        models: Vec<ModelDescriptor>,
        aliases: Vec<ModelAliasDescriptor>,
        providers: Vec<ProviderDescriptor>,
        cached: bool,
        truncated: bool,
    },
    SettingsListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        settings: Vec<UserSettingDescriptor>,
    },
    McpServersListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        servers: Vec<McpServerDescriptor>,
    },
    RuntimeServicesListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        services: Vec<RuntimeServiceDescriptor>,
    },
    McpServerApprovalReviewed {
        meta: CommandAckMeta,
        session_id: SessionId,
        review: McpApprovalReview,
    },
    PermissionsListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        permissions: PermissionStateDescriptor,
    },
    ProviderAuthStarted {
        meta: CommandAckMeta,
        session_id: SessionId,
        attempt_id: ProviderAuthAttemptId,
        provider: String,
        challenge: ProviderAuthChallenge,
        warnings: Vec<String>,
    },
    ProviderConfigured {
        meta: CommandAckMeta,
        session_id: SessionId,
        provider: String,
        auth_kind: ProviderAuthKind,
    },
    ProviderAuthFinished {
        meta: CommandAckMeta,
        session_id: SessionId,
        attempt_id: ProviderAuthAttemptId,
        provider: String,
        success: bool,
        message: String,
        warnings: Vec<String>,
    },
    ProviderActivationFinished {
        meta: CommandAckMeta,
        session_id: SessionId,
        provider: String,
        success: bool,
        message: String,
    },
    WorkspaceFilesFound {
        meta: CommandAckMeta,
        session_id: SessionId,
        matches: Vec<WorkspaceFileMatch>,
        truncated: bool,
    },
    WorkspaceFilePreviewReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        preview: WorkspaceFilePreview,
    },
    WorkspaceStatusReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        status: WorkspaceStatus,
    },
    WorkspaceDiffReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        diff: WorkspaceDiff,
    },
    HostShutdown {
        meta: CommandAckMeta,
    },
    SessionCreated {
        meta: EventMeta,
        driver_client_id: ClientId,
    },
    WorkspaceRootsChanged {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        generation: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        effective_from_turn: u64,
        roots: Vec<WorkspaceRootDescriptor>,
    },
    DriverChanged {
        meta: EventMeta,
        driver_client_id: ClientId,
    },
    MessageQueued {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        position: u64,
        content: String,
        attachments: Vec<StoredAttachment>,
    },
    QueuedMessageRemoved {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        position: u64,
    },
    QueuedMessagesCleared {
        meta: EventMeta,
    },
    UserMessageAccepted {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        agent_turn: u64,
        content: String,
        attachments: Vec<StoredAttachment>,
    },
    /// Human-facing title selected asynchronously after the first successful
    /// assistant turn. This is durable so replay and session lists agree.
    SessionTitleUpdated {
        meta: EventMeta,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        cost: Option<Cost>,
    },
    /// A plugin-originated user message was admitted through the bounded
    /// machine boundary. The ordinary message/turn events remain authoritative
    /// for conversation reconstruction.
    PluginMessageInjected {
        meta: EventMeta,
        plugin_id: String,
        content: String,
        queued: bool,
    },
    /// Session-local status text published by an approved plugin.
    PluginStatusChanged {
        meta: EventMeta,
        plugin_id: String,
        status: String,
    },
    /// An actor-authorized extension state transaction committed to this session.
    ExtensionStateCommitted {
        meta: EventMeta,
        plugin_id: String,
        transaction: crate::extension_contract::ExtensionStateTransaction,
    },
    /// Session-local UI notification published by an approved plugin.
    UiNotification {
        meta: EventMeta,
        plugin_id: String,
        title: String,
        message: String,
    },
    ConversationContextCommitted {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        agent_turn: u64,
        selection: crate::conversation_input::ContextSelection,
    },
    ConversationInputCommitted {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        agent_turn: u64,
        accepted_source: SequenceId,
        selection: crate::conversation_input::InputSelection,
    },
    ConversationTurnCommitted {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        agent_turn: u64,
        turn: crate::Turn,
    },
    ConversationRewound {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        to_agent_turn: u64,
        operation_id: String,
        unrestorable_paths: Vec<UnrestorablePath>,
    },
    TurnStarted {
        meta: EventMeta,
        turn_id: TurnId,
    },
    TextDelta {
        meta: EventMeta,
        turn_id: TurnId,
        text: String,
    },
    ThinkingDelta {
        meta: EventMeta,
        turn_id: TurnId,
        text: String,
        signature: Option<String>,
    },
    CitationDelta {
        meta: EventMeta,
        turn_id: TurnId,
        uri: String,
        title: Option<String>,
    },
    /// Replaceable operational display state, excluded from the durable journal.
    ToolProgress {
        session_id: SessionId,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        progress: rw_operation_contract::ToolProgress,
    },
    ToolCallStarted {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        name: String,
        args: Value,
        call_index: u32,
    },
    ToolApprovalResolved {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        decision: super::ApprovalDecision,
    },
    ToolApprovalNeeded {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        name: String,
        args: Value,
        capabilities: Vec<ToolCapability>,
        rationale: String,
        diff: Option<UnifiedDiff>,
    },
    /// Redacted mutation preview retained independently of whether the active
    /// permission mode needed to ask the user. This keeps inline review
    /// available under remembered approvals and YOLO without coupling display
    /// state to a permission dialog.
    ToolDiffReady {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        diff: UnifiedDiff,
    },
    ToolOutputDelta {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        stream: ToolOutputStream,
        chunk: String,
    },
    ToolCallFinished {
        #[ts(optional = false)]
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(
            schema_with = "crate::schema::required_nullable::<crate::extension_ui::UiPresentation>"
        )]
        presentation: Option<crate::extension_ui::UiPresentation>,
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        output: ToolOutput,
        is_error: bool,
        call_index: u32,
    },
    QuestionAsked {
        meta: EventMeta,
        turn_id: TurnId,
        question_id: QuestionId,
        questions: Vec<Question>,
    },
    QuestionAnswered {
        meta: EventMeta,
        turn_id: TurnId,
        question_id: QuestionId,
        answers: Vec<Answer>,
    },
    ProviderCallAccounted {
        meta: EventMeta,
        call: ProviderCallIdentity,
        actuals: ProviderCallActuals,
    },
    TurnFinished {
        meta: EventMeta,
        turn_id: TurnId,
        status: TurnStatus,
        usage: Usage,
        cost: Cost,
    },
    ContextUsageUpdated {
        meta: EventMeta,
        turn_id: TurnId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        used_tokens: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        usable_tokens: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        reserved_tokens: u64,
        /// False when zero capacity means unknown rather than exhausted.
        context_window_known: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        context_window_reason: Option<String>,
        stable_prefix_hash: String,
        cache_hit_basis_points: u16,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        estimated_input_tokens: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        provider_input_tokens: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        correction_millionths: u64,
    },
    BudgetStatusChanged {
        meta: EventMeta,
        turn_id: TurnId,
        level: BudgetLevel,
        scope: BudgetScope,
        unit: BudgetUnit,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        current: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        limit: u64,
    },
    CompactionStarted {
        meta: EventMeta,
        reason: CompactionReason,
    },
    /// Connection-scoped boundary for one compaction provider attempt.
    /// This never enters the durable session sequence.
    CompactionAttemptStarted {
        started: SequenceId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        revision: u64,
        session_id: SessionId,
        summary_turn_id: TurnId,
        attempt: u32,
    },
    /// Connection-scoped provider text produced while a compaction summary is
    /// generated. This never enters the durable session sequence.
    CompactionTextDelta {
        started: SequenceId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        revision: u64,
        session_id: SessionId,
        summary_turn_id: TurnId,
        attempt: u32,
        text: String,
    },
    /// Connection-scoped provider reasoning produced during compaction.
    CompactionThinkingDelta {
        started: SequenceId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        revision: u64,
        session_id: SessionId,
        summary_turn_id: TurnId,
        attempt: u32,
        text: String,
    },
    /// Accounting for one billed compaction provider attempt that did not
    /// produce the committed summary. This event is deliberately non-terminal:
    /// fallback attempts continue inside the same compaction transaction.
    CompactionAttemptFinished {
        meta: EventMeta,
        summary_turn_id: TurnId,
        usage: Usage,
        cost: Cost,
    },
    CompactionFinished {
        meta: EventMeta,
        summary_turn_id: TurnId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        reclaimed_tokens: u64,
        usage: Option<Usage>,
        cost: Option<Cost>,
    },
    CompactionFailed {
        meta: EventMeta,
        summary_turn_id: TurnId,
    },
    SubagentSpawned {
        meta: EventMeta,
        subagent_id: SubagentId,
        child_session_id: SessionId,
        task: String,
    },
    SubagentFinished {
        meta: EventMeta,
        subagent_id: SubagentId,
        result: SubagentResult,
    },
    /// Connection-scoped child progress. This is never appended to the parent log.
    SubagentProgress {
        parent_session_id: SessionId,
        subagent_id: SubagentId,
        child_session_id: SessionId,
        child_sequence: Option<SequenceId>,
        /// Null invalidates child history at the required canonical `child_sequence`.
        /// A non-null value is a bounded preview; complete content remains in the child log.
        event: Value,
    },
    ToolOutputPruned {
        meta: EventMeta,
        source: crate::ContextBlockId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        reclaimed_tokens: u64,
    },
    ModeChanged {
        meta: EventMeta,
        mode: ModeId,
        /// BLAKE3 hash of the canonical mode semantics.
        definition_fingerprint: String,
    },
    PermissionModeChanged {
        meta: EventMeta,
        /// Session-local override. `None` restores the configured policy.
        mode: Option<String>,
    },
    PlanSubmitted {
        meta: EventMeta,
        artifact: PlanArtifact,
    },
    PlanReviewed {
        meta: EventMeta,
        artifact: PlanArtifact,
        decision: PlanDecision,
        revisions: Option<String>,
    },
    ModelChanged {
        meta: EventMeta,
        model: ModelAlias,
        /// Exact provider route selected for this session, or `None` for the
        /// alias's automatic fallback chain.
        #[serde(default)]
        provider: Option<String>,
        /// Durable per-session effort applied to this selection, including
        /// concrete provider/model routes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        thinking: Option<crate::config::ThinkingLevel>,
    },
    /// The user explicitly chose to start the selected model without prior
    /// conversation. System and project instructions remain available.
    ModelContextCleared {
        meta: EventMeta,
        strategy: ModelContextTransfer,
    },
    ContextItemPinned {
        meta: EventMeta,
        item_id: ContextItemId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        effective_after_agent_turn: u64,
    },
    ContextItemEvicted {
        meta: EventMeta,
        item_id: ContextItemId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        effective_after_agent_turn: u64,
    },
    UserShellStateChanged {
        meta: EventMeta,
        shell_id: ShellId,
        command: Option<String>,
        active: bool,
        status: Option<i32>,
        captured_output: Option<String>,
    },
    HookFailed {
        meta: EventMeta,
        event: String,
        hook_id: String,
        fail_closed: bool,
        message: String,
    },
    CommandFinished {
        meta: EventMeta,
        name: String,
        message: String,
        unrestorable_paths: Vec<UnrestorablePath>,
    },
    GuardTriggered {
        meta: EventMeta,
        turn_id: TurnId,
        guard: String,
        message: String,
    },
    Error {
        meta: EventMeta,
        error: EngineError,
    },
}

impl EngineEvent {
    /// Rebinds connection-scoped receipt metadata without modifying durable event identity.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "Every event must explicitly declare its durable or connection metadata ownership."
    )]
    pub fn command_meta_mut(&mut self) -> Option<&mut CommandAckMeta> {
        match self {
            Self::SessionNavigationRequested { meta, .. }
            | Self::SessionStateReady { meta, .. }
            | Self::SessionControlsReady { meta, .. }
            | Self::FamilyControlsReady { meta, .. }
            | Self::ChildControlsReady { meta, .. }
            | Self::UiCatalogReady { meta, .. }
            | Self::UiPanelsReady { meta, .. }
            | Self::SessionChildrenReady { meta, .. }
            | Self::TodosRead { meta, .. }
            | Self::TranscriptTailReady { meta, .. }
            | Self::TranscriptPageReady { meta, .. }
            | Self::TranscriptContentReady { meta, .. }
            | Self::CommandAcknowledged { meta, .. }
            | Self::ContextSnapshotReady { meta, .. }
            | Self::CostSnapshotReady { meta, .. }
            | Self::SessionReviewReady { meta, .. }
            | Self::SessionReviewUpdated { meta, .. }
            | Self::PromptDumpReady { meta, .. }
            | Self::SessionHistoryReady { meta, .. }
            | Self::SessionReplayCompleted { meta, .. }
            | Self::SessionForked { meta, .. }
            | Self::SessionExported { meta, .. }
            | Self::SessionsListed { meta, .. }
            | Self::SubagentsListed { meta, .. }
            | Self::SessionsSearchReady { meta, .. }
            | Self::CommandDescriptorsListed { meta, .. }
            | Self::ModesListed { meta, .. }
            | Self::ModelsListed { meta, .. }
            | Self::SettingsListed { meta, .. }
            | Self::McpServersListed { meta, .. }
            | Self::RuntimeServicesListed { meta, .. }
            | Self::McpServerApprovalReviewed { meta, .. }
            | Self::PermissionsListed { meta, .. }
            | Self::ProviderAuthStarted { meta, .. }
            | Self::ProviderConfigured { meta, .. }
            | Self::ProviderAuthFinished { meta, .. }
            | Self::ProviderActivationFinished { meta, .. }
            | Self::WorkspaceFilesFound { meta, .. }
            | Self::WorkspaceFilePreviewReady { meta, .. }
            | Self::WorkspaceStatusReady { meta, .. }
            | Self::WorkspaceDiffReady { meta, .. }
            | Self::HostShutdown { meta, .. } => Some(meta),
            Self::TodoStateCommitted { .. }
            | Self::SessionCreated { .. }
            | Self::WorkspaceRootsChanged { .. }
            | Self::DriverChanged { .. }
            | Self::MessageQueued { .. }
            | Self::QueuedMessageRemoved { .. }
            | Self::QueuedMessagesCleared { .. }
            | Self::UserMessageAccepted { .. }
            | Self::SessionTitleUpdated { .. }
            | Self::PluginMessageInjected { .. }
            | Self::PluginStatusChanged { .. }
            | Self::ExtensionStateCommitted { .. }
            | Self::UiNotification { .. }
            | Self::ConversationContextCommitted { .. }
            | Self::ConversationInputCommitted { .. }
            | Self::ConversationTurnCommitted { .. }
            | Self::ConversationRewound { .. }
            | Self::TurnStarted { .. }
            | Self::TextDelta { .. }
            | Self::ThinkingDelta { .. }
            | Self::CitationDelta { .. }
            | Self::ToolProgress { .. }
            | Self::ToolCallStarted { .. }
            | Self::ToolApprovalResolved { .. }
            | Self::ToolApprovalNeeded { .. }
            | Self::ToolDiffReady { .. }
            | Self::ToolOutputDelta { .. }
            | Self::ToolCallFinished { .. }
            | Self::QuestionAsked { .. }
            | Self::QuestionAnswered { .. }
            | Self::ProviderCallAccounted { .. }
            | Self::TurnFinished { .. }
            | Self::ContextUsageUpdated { .. }
            | Self::BudgetStatusChanged { .. }
            | Self::CompactionStarted { .. }
            | Self::CompactionAttemptStarted { .. }
            | Self::CompactionTextDelta { .. }
            | Self::CompactionThinkingDelta { .. }
            | Self::CompactionAttemptFinished { .. }
            | Self::CompactionFinished { .. }
            | Self::CompactionFailed { .. }
            | Self::SubagentSpawned { .. }
            | Self::SubagentFinished { .. }
            | Self::SubagentProgress { .. }
            | Self::ToolOutputPruned { .. }
            | Self::ModeChanged { .. }
            | Self::PermissionModeChanged { .. }
            | Self::PlanSubmitted { .. }
            | Self::PlanReviewed { .. }
            | Self::ModelChanged { .. }
            | Self::ModelContextCleared { .. }
            | Self::ContextItemPinned { .. }
            | Self::ContextItemEvicted { .. }
            | Self::UserShellStateChanged { .. }
            | Self::HookFailed { .. }
            | Self::CommandFinished { .. }
            | Self::GuardTriggered { .. }
            | Self::Error { .. } => None,
        }
    }

    /// Returns the authoritative delivery lifetime for this event variant.
    #[must_use]
    pub fn delivery(&self) -> EngineEventDelivery {
        match self {
            Self::ToolProgress { .. }
            | Self::SubagentProgress { .. }
            | Self::CompactionAttemptStarted { .. }
            | Self::CompactionTextDelta { .. }
            | Self::CompactionThinkingDelta { .. } => EngineEventDelivery::Transient,
            _ if self.meta().is_some() => EngineEventDelivery::Durable,
            _ => EngineEventDelivery::Connection,
        }
    }

    /// Returns durable session metadata. Non-durable events return `None`.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "Every event must explicitly declare its durable or connection metadata ownership."
    )]
    pub fn meta(&self) -> Option<&EventMeta> {
        match self {
            Self::SessionNavigationRequested { .. }
            | Self::TranscriptTailReady { .. }
            | Self::TranscriptPageReady { .. }
            | Self::TranscriptContentReady { .. }
            | Self::SessionStateReady { .. }
            | Self::SessionControlsReady { .. }
            | Self::FamilyControlsReady { .. }
            | Self::ChildControlsReady { .. }
            | Self::UiCatalogReady { .. }
            | Self::UiPanelsReady { .. }
            | Self::SessionChildrenReady { .. }
            | Self::TodosRead { .. }
            | Self::ToolProgress { .. }
            | Self::CommandAcknowledged { .. }
            | Self::ContextSnapshotReady { .. }
            | Self::CostSnapshotReady { .. }
            | Self::SessionReviewReady { .. }
            | Self::SessionReviewUpdated { .. }
            | Self::PromptDumpReady { .. }
            | Self::SessionReplayCompleted { .. }
            | Self::SessionHistoryReady { .. }
            | Self::SessionForked { .. }
            | Self::SessionExported { .. }
            | Self::SessionsListed { .. }
            | Self::SubagentsListed { .. }
            | Self::SessionsSearchReady { .. }
            | Self::CommandDescriptorsListed { .. }
            | Self::ModesListed { .. }
            | Self::ModelsListed { .. }
            | Self::SettingsListed { .. }
            | Self::McpServersListed { .. }
            | Self::RuntimeServicesListed { .. }
            | Self::McpServerApprovalReviewed { .. }
            | Self::PermissionsListed { .. }
            | Self::ProviderAuthStarted { .. }
            | Self::ProviderConfigured { .. }
            | Self::ProviderAuthFinished { .. }
            | Self::ProviderActivationFinished { .. }
            | Self::WorkspaceFilesFound { .. }
            | Self::WorkspaceFilePreviewReady { .. }
            | Self::WorkspaceStatusReady { .. }
            | Self::WorkspaceDiffReady { .. }
            | Self::SubagentProgress { .. }
            | Self::CompactionAttemptStarted { .. }
            | Self::CompactionTextDelta { .. }
            | Self::CompactionThinkingDelta { .. }
            | Self::HostShutdown { .. } => None,
            Self::SessionCreated { meta, .. }
            | Self::WorkspaceRootsChanged { meta, .. }
            | Self::DriverChanged { meta, .. }
            | Self::MessageQueued { meta, .. }
            | Self::QueuedMessageRemoved { meta, .. }
            | Self::QueuedMessagesCleared { meta, .. }
            | Self::UserMessageAccepted { meta, .. }
            | Self::SessionTitleUpdated { meta, .. }
            | Self::PluginMessageInjected { meta, .. }
            | Self::PluginStatusChanged { meta, .. }
            | Self::ExtensionStateCommitted { meta, .. }
            | Self::TodoStateCommitted { meta, .. }
            | Self::UiNotification { meta, .. }
            | Self::ConversationContextCommitted { meta, .. }
            | Self::ConversationInputCommitted { meta, .. }
            | Self::ConversationTurnCommitted { meta, .. }
            | Self::ConversationRewound { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::TextDelta { meta, .. }
            | Self::ThinkingDelta { meta, .. }
            | Self::CitationDelta { meta, .. }
            | Self::ToolCallStarted { meta, .. }
            | Self::ToolApprovalResolved { meta, .. }
            | Self::ToolApprovalNeeded { meta, .. }
            | Self::ToolDiffReady { meta, .. }
            | Self::ToolOutputDelta { meta, .. }
            | Self::ToolCallFinished { meta, .. }
            | Self::QuestionAsked { meta, .. }
            | Self::QuestionAnswered { meta, .. }
            | Self::ProviderCallAccounted { meta, .. }
            | Self::TurnFinished { meta, .. }
            | Self::ContextUsageUpdated { meta, .. }
            | Self::BudgetStatusChanged { meta, .. }
            | Self::CompactionStarted { meta, .. }
            | Self::CompactionAttemptFinished { meta, .. }
            | Self::CompactionFinished { meta, .. }
            | Self::CompactionFailed { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentFinished { meta, .. }
            | Self::ToolOutputPruned { meta, .. }
            | Self::ModeChanged { meta, .. }
            | Self::PermissionModeChanged { meta, .. }
            | Self::PlanSubmitted { meta, .. }
            | Self::PlanReviewed { meta, .. }
            | Self::ModelChanged { meta, .. }
            | Self::ModelContextCleared { meta, .. }
            | Self::ContextItemPinned { meta, .. }
            | Self::ContextItemEvicted { meta, .. }
            | Self::UserShellStateChanged { meta, .. }
            | Self::HookFailed { meta, .. }
            | Self::CommandFinished { meta, .. }
            | Self::GuardTriggered { meta, .. }
            | Self::Error { meta, .. } => Some(meta),
        }
    }

    /// Mutable durable session metadata for storage adapters and validators.
    /// Non-durable events return `None`.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "Every event must explicitly declare its durable or connection metadata ownership."
    )]
    pub fn meta_mut(&mut self) -> Option<&mut EventMeta> {
        match self {
            Self::SessionNavigationRequested { .. }
            | Self::TranscriptTailReady { .. }
            | Self::TranscriptPageReady { .. }
            | Self::TranscriptContentReady { .. }
            | Self::SessionStateReady { .. }
            | Self::SessionControlsReady { .. }
            | Self::FamilyControlsReady { .. }
            | Self::ChildControlsReady { .. }
            | Self::UiCatalogReady { .. }
            | Self::UiPanelsReady { .. }
            | Self::SessionChildrenReady { .. }
            | Self::TodosRead { .. }
            | Self::ToolProgress { .. }
            | Self::CommandAcknowledged { .. }
            | Self::ContextSnapshotReady { .. }
            | Self::CostSnapshotReady { .. }
            | Self::SessionReviewReady { .. }
            | Self::SessionReviewUpdated { .. }
            | Self::PromptDumpReady { .. }
            | Self::SessionReplayCompleted { .. }
            | Self::SessionHistoryReady { .. }
            | Self::SessionForked { .. }
            | Self::SessionExported { .. }
            | Self::SessionsListed { .. }
            | Self::SubagentsListed { .. }
            | Self::SessionsSearchReady { .. }
            | Self::CommandDescriptorsListed { .. }
            | Self::ModesListed { .. }
            | Self::ModelsListed { .. }
            | Self::SettingsListed { .. }
            | Self::McpServersListed { .. }
            | Self::RuntimeServicesListed { .. }
            | Self::McpServerApprovalReviewed { .. }
            | Self::PermissionsListed { .. }
            | Self::ProviderAuthStarted { .. }
            | Self::ProviderConfigured { .. }
            | Self::ProviderAuthFinished { .. }
            | Self::ProviderActivationFinished { .. }
            | Self::WorkspaceFilesFound { .. }
            | Self::WorkspaceFilePreviewReady { .. }
            | Self::WorkspaceStatusReady { .. }
            | Self::WorkspaceDiffReady { .. }
            | Self::SubagentProgress { .. }
            | Self::CompactionAttemptStarted { .. }
            | Self::CompactionTextDelta { .. }
            | Self::CompactionThinkingDelta { .. }
            | Self::HostShutdown { .. } => None,
            Self::SessionCreated { meta, .. }
            | Self::WorkspaceRootsChanged { meta, .. }
            | Self::DriverChanged { meta, .. }
            | Self::MessageQueued { meta, .. }
            | Self::QueuedMessageRemoved { meta, .. }
            | Self::QueuedMessagesCleared { meta, .. }
            | Self::UserMessageAccepted { meta, .. }
            | Self::SessionTitleUpdated { meta, .. }
            | Self::PluginMessageInjected { meta, .. }
            | Self::PluginStatusChanged { meta, .. }
            | Self::ExtensionStateCommitted { meta, .. }
            | Self::TodoStateCommitted { meta, .. }
            | Self::UiNotification { meta, .. }
            | Self::ConversationContextCommitted { meta, .. }
            | Self::ConversationInputCommitted { meta, .. }
            | Self::ConversationTurnCommitted { meta, .. }
            | Self::ConversationRewound { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::TextDelta { meta, .. }
            | Self::ThinkingDelta { meta, .. }
            | Self::CitationDelta { meta, .. }
            | Self::ToolCallStarted { meta, .. }
            | Self::ToolApprovalResolved { meta, .. }
            | Self::ToolApprovalNeeded { meta, .. }
            | Self::ToolDiffReady { meta, .. }
            | Self::ToolOutputDelta { meta, .. }
            | Self::ToolCallFinished { meta, .. }
            | Self::QuestionAsked { meta, .. }
            | Self::QuestionAnswered { meta, .. }
            | Self::ProviderCallAccounted { meta, .. }
            | Self::TurnFinished { meta, .. }
            | Self::ContextUsageUpdated { meta, .. }
            | Self::BudgetStatusChanged { meta, .. }
            | Self::CompactionStarted { meta, .. }
            | Self::CompactionAttemptFinished { meta, .. }
            | Self::CompactionFinished { meta, .. }
            | Self::CompactionFailed { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentFinished { meta, .. }
            | Self::ToolOutputPruned { meta, .. }
            | Self::ModeChanged { meta, .. }
            | Self::PermissionModeChanged { meta, .. }
            | Self::PlanSubmitted { meta, .. }
            | Self::PlanReviewed { meta, .. }
            | Self::ModelChanged { meta, .. }
            | Self::ModelContextCleared { meta, .. }
            | Self::ContextItemPinned { meta, .. }
            | Self::ContextItemEvicted { meta, .. }
            | Self::UserShellStateChanged { meta, .. }
            | Self::HookFailed { meta, .. }
            | Self::CommandFinished { meta, .. }
            | Self::GuardTriggered { meta, .. }
            | Self::Error { meta, .. } => Some(meta),
        }
    }
}
