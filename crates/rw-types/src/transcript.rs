//! Semantic transcript previews. Canonical journal records retain complete bodies.

use crate::{Role, SequenceId, SessionId, SubagentId, SubagentStatus};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Version of the rebuildable semantic transcript projection.
pub const TRANSCRIPT_PROJECTION_VERSION: u32 = 1;
/// Maximum retained text bytes across previews in one semantic item.
pub const TRANSCRIPT_PREVIEW_TEXT_BYTES: usize = 4 * 1024;
/// Maximum inline conversation block descriptors in one semantic item.
pub const TRANSCRIPT_PREVIEW_BLOCKS: usize = 16;

/// Stable item identity, independent of its current logical ordinal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct TranscriptItemId(pub SequenceId);

/// A closed selector into one canonical event. It never names a filesystem path.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum TranscriptContentSelector {
    Conversation,
    ConversationBlock { index: u32 },
    ToolArguments,
    ToolOutput,
    ToolDiff,
    CommandMessage,
    ShellCommand,
    ShellOutput,
    SubagentTask,
    SubagentResult,
}

/// Body location within the exact journal prefix carried by its enclosing view.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct TranscriptContentSource {
    pub sequence: SequenceId,
    pub selector: TranscriptContentSelector,
}

/// Preview syntax. Incomplete JSON is display text and must not be parsed as JSON.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum TranscriptPreviewFormat {
    Text,
    Json,
}

/// A bounded prefix with an authoritative source for the complete body.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct TranscriptBodyPreview {
    pub text: String,
    pub format: TranscriptPreviewFormat,
    pub complete: bool,
    pub source: TranscriptContentSource,
}

/// Displayable conversation content; provider continuation signatures stay in IR.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum TranscriptConversationBlock {
    Text { body: TranscriptBodyPreview },
    Reasoning { body: TranscriptBodyPreview },
    Image { source: TranscriptContentSource },
    Citation { body: TranscriptBodyPreview },
}

/// Tool invocation lifecycle independent of provider call/result IR placement.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum TranscriptToolStatus {
    Running,
    Finished {
        is_error: bool,
        output: TranscriptBodyPreview,
    },
}

/// Child lifecycle without retaining its full historical event projection.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum TranscriptSubagentStatus {
    Running,
    Finished {
        status: SubagentStatus,
        result: TranscriptBodyPreview,
    },
}

/// Bounded, renderer-independent content of one stable logical transcript row.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum TranscriptContent {
    Conversation {
        role: Role,
        blocks: Vec<TranscriptConversationBlock>,
        omitted_blocks: bool,
        source: TranscriptContentSource,
    },
    Tool {
        invocation_id: crate::ToolInvocationId,
        name: String,
        call_index: u32,
        arguments: TranscriptBodyPreview,
        diff: Option<TranscriptBodyPreview>,
        status: TranscriptToolStatus,
    },
    Command {
        name: String,
        message: TranscriptBodyPreview,
    },
    Shell {
        command: Option<TranscriptBodyPreview>,
        output: Option<TranscriptBodyPreview>,
        active: bool,
        status: Option<i32>,
    },
    Subagent {
        subagent_id: SubagentId,
        session_id: SessionId,
        task: TranscriptBodyPreview,
        status: TranscriptSubagentStatus,
    },
}
