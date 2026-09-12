//! Public journal delivery for extensions. Private namespace transactions and
//! host control/accounting records never belong to this event catalog.
use crate::{
    EngineEvent, SequenceId,
    extension_contract::{ExtensionDeliveryCursor, ExtensionStateMutation},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_EXTENSION_EVENT_INLINE_BYTES: usize = 256 * 1024;
pub const MAX_EXTENSION_EVENT_SOURCE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_EXTENSION_EVENT_CHUNK_BYTES: u32 = 64 * 1024;

macro_rules! catalog {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, JsonSchema, TS)]
        pub enum ExtensionEventKind { $(#[serde(rename = $wire)] $variant,)+ }
        impl ExtensionEventKind {
            #[must_use]
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire,)+ } }
            #[must_use]
            pub const fn from_event(event: &EngineEvent) -> Option<Self> {
                match event { $(EngineEvent::$variant { .. } => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}
catalog! {
    SessionCreated => "session_created",
    WorkspaceRootsChanged => "workspace_roots_changed",
    UserMessageAccepted => "user_message_accepted",
    SessionTitleUpdated => "session_title_updated",
    ConversationRewound => "conversation_rewound",
    TurnStarted => "turn_started",
    ToolCallStarted => "tool_call_started",
    ToolCallFinished => "tool_call_finished",
    TurnFinished => "turn_finished",
    CompactionFinished => "compaction_finished",
    SubagentSpawned => "subagent_spawned",
    SubagentFinished => "subagent_finished",
    ModeChanged => "mode_changed",
    ModelChanged => "model_changed",
    HookFailed => "hook_failed",
    CommandFinished => "command_finished",
    DriverChanged => "driver_changed",
    MessageQueued => "message_queued",
    QueuedMessageRemoved => "queued_message_removed",
    QueuedMessagesCleared => "queued_messages_cleared",
    PluginMessageInjected => "plugin_message_injected",
    PluginStatusChanged => "plugin_status_changed",
    UiNotification => "ui_notification",
    TextDelta => "text_delta",
    ThinkingDelta => "thinking_delta",
    CitationDelta => "citation_delta",
    ToolApprovalNeeded => "tool_approval_needed",
    ToolDiffReady => "tool_diff_ready",
    ToolOutputDelta => "tool_output_delta",
    QuestionAsked => "question_asked",
    QuestionAnswered => "question_answered",
    ContextUsageUpdated => "context_usage_updated",
    BudgetStatusChanged => "budget_status_changed",
    CompactionStarted => "compaction_started",
    CompactionAttemptStarted => "compaction_attempt_started",
    CompactionTextDelta => "compaction_text_delta",
    CompactionThinkingDelta => "compaction_thinking_delta",
    CompactionAttemptFinished => "compaction_attempt_finished",
    CompactionFailed => "compaction_failed",
    SubagentProgress => "subagent_progress",
    ToolOutputPruned => "tool_output_pruned",
    PermissionModeChanged => "permission_mode_changed",
    PlanSubmitted => "plan_submitted",
    PlanReviewed => "plan_reviewed",
    ModelContextCleared => "model_context_cleared",
    ContextItemPinned => "context_item_pinned",
    ContextItemEvicted => "context_item_evicted",
    UserShellStateChanged => "user_shell_state_changed",
    GuardTriggered => "guard_triggered",
    Error => "error",

}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS)]
#[serde(tag = "storage", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionEventContent {
    Inline {
        data: serde_json::Value,
    },
    /// Only the active delivery cursor authorizes source reads. The source is
    /// redacted canonical event JSON, retained until delivery settles.
    Source {
        #[schemars(range(min = 1, max = 33_554_432))]
        bytes: u32,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEventNotice {
    pub cursor: ExtensionDeliveryCursor,
    pub event: ExtensionEventKind,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    pub state_revision: Option<SequenceId>,
    pub content: ExtensionEventContent,
}

/// The host binds these mutations to the delivered cursor and observed revision
/// and durably commits their acknowledgement together. Handler completion alone
/// does not advance delivery. External effects have at-least-once semantics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEventOutcome {
    #[schemars(length(max = 32))]
    pub mutations: Vec<ExtensionStateMutation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEventRead {
    pub cursor: ExtensionDeliveryCursor,
    pub offset: u32,
    #[schemars(range(min = 1, max = 65_536))]
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEventChunk {
    pub cursor: ExtensionDeliveryCursor,
    pub offset: u32,
    pub data_base64: String,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<u32>")]
    pub next_offset: Option<u32>,
}

impl ExtensionEventNotice {
    /// # Errors
    /// Rejects invalid content envelopes; cursor identity is supplied by the host.
    pub fn validate(&self) -> Result<(), &'static str> {
        match &self.content {
            ExtensionEventContent::Inline { data } => {
                let mut count = InlineBytes(0);
                if serde_json::to_writer(&mut count, data).is_err() || !data.is_object() {
                    return Err("extension event data shape");
                }
            }
            ExtensionEventContent::Source { bytes } => {
                if *bytes == 0 || u64::from(*bytes) > MAX_EXTENSION_EVENT_SOURCE_BYTES as u64 {
                    return Err("extension source size");
                }
            }
        }
        Ok(())
    }
}

struct InlineBytes(usize);
impl std::io::Write for InlineBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|length| *length <= MAX_EXTENSION_EVENT_INLINE_BYTES)
            .ok_or_else(|| std::io::Error::other("inline event byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::ExtensionEventKind;
    use serde_json::json;
    #[test]
    fn public_catalog_rejects_private_state_and_alternate_names() {
        for name in [
            "ExtensionStateCommitted",
            "extension_state_committed",
            "TurnFinished",
            "provider_call_accounted",
            "command_acknowledged",
        ] {
            assert!(serde_json::from_value::<ExtensionEventKind>(json!(name)).is_err());
        }
        assert_eq!(
            serde_json::from_value::<ExtensionEventKind>(json!("turn_finished")).ok(),
            Some(ExtensionEventKind::TurnFinished)
        );
    }
}
