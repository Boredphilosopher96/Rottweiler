use super::{
    Answer, ApprovalBinding, ApprovalDecision, Attachment, ClientRole, CommandMeta, CommandOutcome,
    ContextItemId, EngineEvent, McpEnvironmentEntry, ModeId, ModelAlias, PermissionApprovalScope,
    PlanDecision, ProviderAuthAttemptId, QuestionId, ReviewFileDecision, RewindTarget, SequenceId,
    SessionId, ShellId, SubagentId, TranscriptFormat, TurnId,
};
use crate::{ToolCallId, ToolInvocationId, config::PermissionDecision};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Commands accepted by the headless engine from any client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case", optional_fields = nullable)]
#[serde(deny_unknown_fields)]
pub enum ClientCommand {
    ReadTranscript {
        meta: CommandMeta,
        session_id: SessionId,
        read: crate::transcript::TranscriptRead,
    },
    ReadTranscriptContent {
        meta: CommandMeta,
        session_id: SessionId,
        read: crate::transcript::TranscriptContentRead,
    },
    CreateSession {
        meta: CommandMeta,
        cwd: String,
        model: Option<ModelAlias>,
    },
    ResumeSession {
        meta: CommandMeta,
        session_id: SessionId,
        last_seen_sequence: Option<SequenceId>,
        role: ClientRole,
    },
    AttachSession {
        meta: CommandMeta,
        session_id: SessionId,
        last_seen_sequence: Option<SequenceId>,
        role: ClientRole,
    },
    SendMessage {
        meta: CommandMeta,
        session_id: SessionId,
        content: String,
        attachments: Vec<Attachment>,
    },
    Interrupt {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ApproveTool {
        meta: CommandMeta,
        session_id: SessionId,
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        decision: ApprovalDecision,
        /// Required when the pending approval displayed a unified diff. The
        /// actor rejects missing or stale bindings without consuming the ask.
        binding: Option<ApprovalBinding>,
    },
    ApprovePlan {
        meta: CommandMeta,
        session_id: SessionId,
        decision: PlanDecision,
        revisions: Option<String>,
    },
    AnswerQuestion {
        meta: CommandMeta,
        session_id: SessionId,
        question_id: QuestionId,
        answers: Vec<Answer>,
    },
    SwitchMode {
        meta: CommandMeta,
        session_id: SessionId,
        mode: ModeId,
    },
    SwitchModel {
        meta: CommandMeta,
        session_id: SessionId,
        model: ModelAlias,
        /// When set, route only through this configured provider for the
        /// selected alias instead of using the alias's automatic fallback chain.
        #[serde(default)]
        provider: Option<String>,
    },
    Compact {
        meta: CommandMeta,
        session_id: SessionId,
        instructions: Option<String>,
    },
    Fork {
        meta: CommandMeta,
        session_id: SessionId,
        at_turn: Option<TurnId>,
        /// Stable client-generated identity retained until the correlated fork completes.
        operation_id: String,
    },
    Rewind {
        meta: CommandMeta,
        session_id: SessionId,
        target: RewindTarget,
    },
    TakeDriver {
        meta: CommandMeta,
        session_id: SessionId,
    },
    UserShellStarted {
        meta: CommandMeta,
        session_id: SessionId,
        command: String,
    },
    UserShellEnded {
        meta: CommandMeta,
        session_id: SessionId,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    },
    AttachDevelopmentPlugin {
        meta: CommandMeta,
        session_id: SessionId,
        source: String,
    },
    DetachDevelopmentPlugin {
        meta: CommandMeta,
        session_id: SessionId,
    },
    PinContext {
        meta: CommandMeta,
        session_id: SessionId,
        item_id: ContextItemId,
    },
    EvictContext {
        meta: CommandMeta,
        session_id: SessionId,
        item_id: ContextItemId,
    },
    GetContext {
        meta: CommandMeta,
        session_id: SessionId,
    },
    GetCost {
        meta: CommandMeta,
        session_id: SessionId,
    },
    GetSessionReview {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ReviewFile {
        meta: CommandMeta,
        session_id: SessionId,
        path: String,
        decision: ReviewFileDecision,
        current_hash: String,
    },
    DumpPrompt {
        meta: CommandMeta,
        session_id: SessionId,
        turn_id: Option<TurnId>,
    },
    ListSessions {
        meta: CommandMeta,
    },
    SearchSessions {
        meta: CommandMeta,
        query: String,
        limit: u32,
    },
    ListCommands {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ListModes {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ListModels {
        meta: CommandMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session_id: Option<SessionId>,
        refresh: bool,
    },
    ListSettings {
        meta: CommandMeta,
        session_id: SessionId,
    },
    SetSetting {
        meta: CommandMeta,
        session_id: SessionId,
        key: String,
        value: String,
    },
    ListMcpServers {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ListRuntimeServices {
        meta: CommandMeta,
        session_id: SessionId,
    },
    AddMcpHttpServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
        endpoint: String,
    },
    AddMcpStdioServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
        executable: String,
        args: Vec<String>,
        environment: Vec<McpEnvironmentEntry>,
    },
    RemoveMcpServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
    },
    ReviewMcpServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
    },
    ApproveMcpServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
        fingerprint: String,
    },
    SetMcpServerEnabled {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
        enabled: bool,
    },
    ListPermissions {
        meta: CommandMeta,
        session_id: SessionId,
    },
    AddSessionPermissionRule {
        meta: CommandMeta,
        session_id: SessionId,
        pattern: String,
        action: PermissionDecision,
    },
    RemoveSessionPermissionRule {
        meta: CommandMeta,
        session_id: SessionId,
        rule_id: String,
    },
    RemoveQueuedMessage {
        meta: CommandMeta,
        session_id: SessionId,
        position: String,
    },
    ClearQueuedMessages {
        meta: CommandMeta,
        session_id: SessionId,
    },
    RenameSession {
        meta: CommandMeta,
        session_id: SessionId,
        title: String,
    },
    ExportSession {
        meta: CommandMeta,
        session_id: SessionId,
        format: TranscriptFormat,
        output_path: String,
        force: bool,
    },
    RevokePermissionApproval {
        meta: CommandMeta,
        session_id: SessionId,
        approval_id: String,
        scope: PermissionApprovalScope,
    },
    BeginProviderAuth {
        meta: CommandMeta,
        session_id: SessionId,
        provider: String,
    },
    ConfigureBuiltinProvider {
        meta: CommandMeta,
        session_id: SessionId,
        provider: String,
    },
    CompleteProviderAuth {
        meta: CommandMeta,
        session_id: SessionId,
        provider: String,
        attempt_id: ProviderAuthAttemptId,
    },
    CancelProviderAuth {
        meta: CommandMeta,
        session_id: SessionId,
        provider: String,
        attempt_id: ProviderAuthAttemptId,
    },
    SearchWorkspaceFiles {
        meta: CommandMeta,
        session_id: SessionId,
        query: String,
        limit: u32,
    },
    PreviewWorkspaceFile {
        meta: CommandMeta,
        session_id: SessionId,
        path: String,
        max_bytes: u32,
    },
    GetWorkspaceStatus {
        meta: CommandMeta,
        session_id: SessionId,
    },
    GetWorkspaceDiff {
        meta: CommandMeta,
        session_id: SessionId,
        path: String,
        max_bytes: u32,
    },
    ListSubagents {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ContinueSubagent {
        meta: CommandMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
        content: String,
    },
    InterruptSubagent {
        meta: CommandMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
    },
    CloseSubagent {
        meta: CommandMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
    },
    ShutdownHost {
        meta: CommandMeta,
    },
}

impl ClientCommand {
    /// Returns caller-supplied command metadata. Transports must replace the
    /// client id with their authenticated, connection-bound identity before
    /// authorization or dispatch.
    #[must_use]
    pub fn meta(&self) -> &CommandMeta {
        match self {
            Self::ReadTranscript { meta, .. }
            | Self::ReadTranscriptContent { meta, .. }
            | Self::CreateSession { meta, .. }
            | Self::ResumeSession { meta, .. }
            | Self::AttachSession { meta, .. }
            | Self::SendMessage { meta, .. }
            | Self::Interrupt { meta, .. }
            | Self::ApproveTool { meta, .. }
            | Self::ApprovePlan { meta, .. }
            | Self::AnswerQuestion { meta, .. }
            | Self::SwitchMode { meta, .. }
            | Self::SwitchModel { meta, .. }
            | Self::Compact { meta, .. }
            | Self::Fork { meta, .. }
            | Self::Rewind { meta, .. }
            | Self::TakeDriver { meta, .. }
            | Self::UserShellStarted { meta, .. }
            | Self::UserShellEnded { meta, .. }
            | Self::AttachDevelopmentPlugin { meta, .. }
            | Self::DetachDevelopmentPlugin { meta, .. }
            | Self::PinContext { meta, .. }
            | Self::EvictContext { meta, .. }
            | Self::GetContext { meta, .. }
            | Self::GetCost { meta, .. }
            | Self::GetSessionReview { meta, .. }
            | Self::ReviewFile { meta, .. }
            | Self::DumpPrompt { meta, .. }
            | Self::ListSessions { meta, .. }
            | Self::SearchSessions { meta, .. }
            | Self::ListCommands { meta, .. }
            | Self::ListModes { meta, .. }
            | Self::ListModels { meta, .. }
            | Self::ListSettings { meta, .. }
            | Self::SetSetting { meta, .. }
            | Self::ListMcpServers { meta, .. }
            | Self::ListRuntimeServices { meta, .. }
            | Self::AddMcpHttpServer { meta, .. }
            | Self::AddMcpStdioServer { meta, .. }
            | Self::RemoveMcpServer { meta, .. }
            | Self::ReviewMcpServer { meta, .. }
            | Self::ApproveMcpServer { meta, .. }
            | Self::SetMcpServerEnabled { meta, .. }
            | Self::ListPermissions { meta, .. }
            | Self::AddSessionPermissionRule { meta, .. }
            | Self::RemoveSessionPermissionRule { meta, .. }
            | Self::RemoveQueuedMessage { meta, .. }
            | Self::ClearQueuedMessages { meta, .. }
            | Self::RenameSession { meta, .. }
            | Self::ExportSession { meta, .. }
            | Self::RevokePermissionApproval { meta, .. }
            | Self::BeginProviderAuth { meta, .. }
            | Self::ConfigureBuiltinProvider { meta, .. }
            | Self::CompleteProviderAuth { meta, .. }
            | Self::CancelProviderAuth { meta, .. }
            | Self::SearchWorkspaceFiles { meta, .. }
            | Self::PreviewWorkspaceFile { meta, .. }
            | Self::GetWorkspaceStatus { meta, .. }
            | Self::GetWorkspaceDiff { meta, .. }
            | Self::ListSubagents { meta, .. }
            | Self::ContinueSubagent { meta, .. }
            | Self::InterruptSubagent { meta, .. }
            | Self::CloseSubagent { meta, .. }
            | Self::ShutdownHost { meta, .. } => meta,
        }
    }

    /// Returns the target session for session-scoped commands.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::CreateSession { .. }
            | Self::ListSessions { .. }
            | Self::SearchSessions { .. }
            | Self::ListModels { .. }
            | Self::ShutdownHost { .. } => None,
            Self::ReadTranscript { session_id, .. }
            | Self::ReadTranscriptContent { session_id, .. }
            | Self::ResumeSession { session_id, .. }
            | Self::AttachSession { session_id, .. }
            | Self::SendMessage { session_id, .. }
            | Self::Interrupt { session_id, .. }
            | Self::ApproveTool { session_id, .. }
            | Self::ApprovePlan { session_id, .. }
            | Self::AnswerQuestion { session_id, .. }
            | Self::SwitchMode { session_id, .. }
            | Self::SwitchModel { session_id, .. }
            | Self::Compact { session_id, .. }
            | Self::Fork { session_id, .. }
            | Self::Rewind { session_id, .. }
            | Self::TakeDriver { session_id, .. }
            | Self::UserShellStarted { session_id, .. }
            | Self::UserShellEnded { session_id, .. }
            | Self::AttachDevelopmentPlugin { session_id, .. }
            | Self::DetachDevelopmentPlugin { session_id, .. }
            | Self::PinContext { session_id, .. }
            | Self::EvictContext { session_id, .. }
            | Self::GetContext { session_id, .. }
            | Self::GetCost { session_id, .. }
            | Self::DumpPrompt { session_id, .. }
            | Self::GetSessionReview { session_id, .. }
            | Self::ReviewFile { session_id, .. }
            | Self::SearchWorkspaceFiles { session_id, .. }
            | Self::PreviewWorkspaceFile { session_id, .. }
            | Self::GetWorkspaceStatus { session_id, .. }
            | Self::GetWorkspaceDiff { session_id, .. }
            | Self::ListCommands { session_id, .. }
            | Self::ListModes { session_id, .. }
            | Self::ListSettings { session_id, .. }
            | Self::SetSetting { session_id, .. }
            | Self::ListMcpServers { session_id, .. }
            | Self::ListRuntimeServices { session_id, .. }
            | Self::AddMcpHttpServer { session_id, .. }
            | Self::AddMcpStdioServer { session_id, .. }
            | Self::RemoveMcpServer { session_id, .. }
            | Self::ReviewMcpServer { session_id, .. }
            | Self::ApproveMcpServer { session_id, .. }
            | Self::SetMcpServerEnabled { session_id, .. }
            | Self::ListPermissions { session_id, .. }
            | Self::AddSessionPermissionRule { session_id, .. }
            | Self::RemoveSessionPermissionRule { session_id, .. }
            | Self::RemoveQueuedMessage { session_id, .. }
            | Self::ClearQueuedMessages { session_id, .. }
            | Self::RenameSession { session_id, .. }
            | Self::ExportSession { session_id, .. }
            | Self::RevokePermissionApproval { session_id, .. }
            | Self::BeginProviderAuth { session_id, .. }
            | Self::ConfigureBuiltinProvider { session_id, .. }
            | Self::CompleteProviderAuth { session_id, .. }
            | Self::CancelProviderAuth { session_id, .. }
            | Self::ListSubagents { session_id, .. }
            | Self::ContinueSubagent { session_id, .. }
            | Self::InterruptSubagent { session_id, .. }
            | Self::CloseSubagent { session_id, .. } => Some(session_id),
        }
    }

    /// Mutable metadata used by a transport to bind authorization to its
    /// authenticated connection instead of trusting the wire `client_id`.
    #[must_use]
    pub fn meta_mut(&mut self) -> &mut CommandMeta {
        match self {
            Self::ReadTranscript { meta, .. }
            | Self::ReadTranscriptContent { meta, .. }
            | Self::CreateSession { meta, .. }
            | Self::ResumeSession { meta, .. }
            | Self::AttachSession { meta, .. }
            | Self::SendMessage { meta, .. }
            | Self::Interrupt { meta, .. }
            | Self::ApproveTool { meta, .. }
            | Self::ApprovePlan { meta, .. }
            | Self::AnswerQuestion { meta, .. }
            | Self::SwitchMode { meta, .. }
            | Self::SwitchModel { meta, .. }
            | Self::Compact { meta, .. }
            | Self::Fork { meta, .. }
            | Self::Rewind { meta, .. }
            | Self::TakeDriver { meta, .. }
            | Self::UserShellStarted { meta, .. }
            | Self::UserShellEnded { meta, .. }
            | Self::AttachDevelopmentPlugin { meta, .. }
            | Self::DetachDevelopmentPlugin { meta, .. }
            | Self::PinContext { meta, .. }
            | Self::EvictContext { meta, .. }
            | Self::GetContext { meta, .. }
            | Self::GetCost { meta, .. }
            | Self::GetSessionReview { meta, .. }
            | Self::ReviewFile { meta, .. }
            | Self::DumpPrompt { meta, .. }
            | Self::ListSessions { meta, .. }
            | Self::SearchSessions { meta, .. }
            | Self::ListCommands { meta, .. }
            | Self::ListModes { meta, .. }
            | Self::ListModels { meta, .. }
            | Self::ListSettings { meta, .. }
            | Self::SetSetting { meta, .. }
            | Self::ListMcpServers { meta, .. }
            | Self::ListRuntimeServices { meta, .. }
            | Self::AddMcpHttpServer { meta, .. }
            | Self::AddMcpStdioServer { meta, .. }
            | Self::RemoveMcpServer { meta, .. }
            | Self::ReviewMcpServer { meta, .. }
            | Self::ApproveMcpServer { meta, .. }
            | Self::SetMcpServerEnabled { meta, .. }
            | Self::ListPermissions { meta, .. }
            | Self::AddSessionPermissionRule { meta, .. }
            | Self::RemoveSessionPermissionRule { meta, .. }
            | Self::RemoveQueuedMessage { meta, .. }
            | Self::ClearQueuedMessages { meta, .. }
            | Self::RenameSession { meta, .. }
            | Self::ExportSession { meta, .. }
            | Self::RevokePermissionApproval { meta, .. }
            | Self::BeginProviderAuth { meta, .. }
            | Self::ConfigureBuiltinProvider { meta, .. }
            | Self::CompleteProviderAuth { meta, .. }
            | Self::CancelProviderAuth { meta, .. }
            | Self::SearchWorkspaceFiles { meta, .. }
            | Self::PreviewWorkspaceFile { meta, .. }
            | Self::GetWorkspaceStatus { meta, .. }
            | Self::GetWorkspaceDiff { meta, .. }
            | Self::ListSubagents { meta, .. }
            | Self::ContinueSubagent { meta, .. }
            | Self::InterruptSubagent { meta, .. }
            | Self::CloseSubagent { meta, .. }
            | Self::ShutdownHost { meta, .. } => meta,
        }
    }
}

/// Host queries return directly; actor/control operations use durable command semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandExecution {
    Read,
    Control,
}
// One variant list owns native classification and its generated wire projection.
macro_rules! read_commands {
    ($($variant:ident),+ $(,)?) => {
        impl ClientCommand {
            /// Classify execution without decoding or serializing the command again.
            #[must_use]
            pub const fn execution(&self) -> CommandExecution {
                match self {
                    $(Self::$variant { .. })|+ => CommandExecution::Read,
                    _ => CommandExecution::Control,
                }
            }

            /// Source-owned read tags for schema/code generation.
            pub fn read_type_tags() -> impl Serialize {
                #[derive(Serialize)]
                #[serde(rename_all = "snake_case")]
                enum Tag { $($variant),+ }
                [$(Tag::$variant),+]
            }
        }
    };
}
read_commands!(
    ReadTranscript,
    ReadTranscriptContent,
    ListSessions,
    SearchSessions,
    ListCommands,
    ListModes,
    ListModels,
    ListSettings,
    ListMcpServers,
    ListRuntimeServices,
    SearchWorkspaceFiles,
    PreviewWorkspaceFile,
    GetWorkspaceStatus,
    GetWorkspaceDiff,
    ListSubagents,
);

/// Direct response on the authenticated command channel. Read data never enters SSE or mutation dedupe.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum CommandReply {
    Command {
        outcome: CommandOutcome,
    },
    Read {
        outcome: CommandOutcome,
        events: Vec<EngineEvent>,
    },
}
impl CommandReply {
    #[must_use]
    pub fn outcome(&self) -> &CommandOutcome {
        match self {
            Self::Command { outcome } | Self::Read { outcome, .. } => outcome,
        }
    }
}
/// Maximum in-flight reads per authenticated client, held through reply body release.
pub const MAX_CLIENT_READS: usize = 2;

/// Maximum encoded command reply, including its envelope.
pub const MAX_COMMAND_REPLY_BYTES: usize = 8 * 1024 * 1024;
