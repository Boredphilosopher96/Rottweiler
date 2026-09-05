//! Semantic transcript previews. Canonical journal records retain complete bodies.

use crate::{Role, SequenceId, SessionId, SubagentId, SubagentStatus};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Version of the rebuildable semantic transcript projection.
pub const TRANSCRIPT_PROJECTION_VERSION: u32 = 6;
/// Maximum retained text bytes across previews in one semantic item.
pub const TRANSCRIPT_PREVIEW_TEXT_BYTES: usize = 4 * 1024;
/// Maximum inline conversation block descriptors in one semantic item.
pub const TRANSCRIPT_PREVIEW_BLOCKS: usize = 16;

/// Stable item identity, independent of its current logical ordinal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
pub struct TranscriptItemId(pub SequenceId);

/// A closed selector into one canonical event. It never names a filesystem path.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum TranscriptContentSelector {
    Conversation {},
    ConversationBlock {
        index: u32,
    },
    ToolArguments {},
    ToolOutput {},
    ToolDiff {},
    ToolPresentation {
        invocation_id: crate::ToolInvocationId,
    },
    CommandMessage {},
    ShellCommand {},
    ShellOutput {},
    SubagentTask {},
    SubagentResult {},
    SubagentDiff {},
}

/// Body location within the exact journal prefix carried by its enclosing view.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptContentSource {
    pub sequence: SequenceId,
    pub selector: TranscriptContentSelector,
}

/// Preview syntax. Incomplete JSON is display text and must not be parsed as JSON.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum TranscriptPreviewFormat {
    Text,
    Json,
}

/// A bounded prefix with an authoritative source for the complete body.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum TranscriptConversationBlock {
    Text { body: TranscriptBodyPreview },
    Reasoning { body: TranscriptBodyPreview },
    Image { source: TranscriptContentSource },
    Citation { body: TranscriptBodyPreview },
}

/// Compact reference to a complete host-projected tool surface.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptToolPresentation {
    #[schemars(length(max = 128), extend("x-rw-max-utf8-bytes" = 128))]
    pub title: String,
    pub source: TranscriptContentSource,
}

/// Tool invocation lifecycle independent of provider call/result IR placement.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum TranscriptToolStatus {
    Running {},
    Finished {
        is_error: bool,
        output: TranscriptBodyPreview,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<TranscriptToolPresentation>")]
        #[ts(optional = false)]
        presentation: Option<TranscriptToolPresentation>,
    },
}

/// Child lifecycle without retaining its full historical event projection.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum TranscriptSubagentStatus {
    Running {},
    Finished {
        status: SubagentStatus,
        result: TranscriptBodyPreview,
        touched_file_count: u32,
        diff: Option<TranscriptContentSource>,
    },
}

/// Bounded, renderer-independent content of one stable logical transcript row.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum TranscriptContent {
    TurnSummary {
        turn_id: crate::TurnId,
        status: crate::TurnStatus,
        usage: crate::Usage,
        cost: crate::Cost,
    },
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

/// Logical row position, distinct from a canonical event sequence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
pub struct TranscriptOrdinal(
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub u64,
);

/// Current semantic ordering generation, changed by rewind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
pub struct TranscriptGeneration(
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub u64,
);

/// Exact canonical prefix interpreted by a complete semantic view.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptView {
    pub session_id: SessionId,
    pub projection_version: u32,
    pub generation: TranscriptGeneration,
    pub through: Option<SequenceId>,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
#[derive(Allocation)]
pub enum TranscriptPosition {
    First {},
    Latest {},
    Before {
        item: TranscriptItemId,
    },
    After {
        item: TranscriptItemId,
    },
    Around {
        item: TranscriptItemId,
    },
    AtOrdinal {
        ordinal: TranscriptOrdinal,
        generation: TranscriptGeneration,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[derive(Allocation)]
pub struct TranscriptRead {
    pub known_view: Option<TranscriptView>,
    pub position: TranscriptPosition,
    pub max_items: u32,
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptItem {
    pub id: TranscriptItemId,
    pub ordinal: TranscriptOrdinal,
    pub revision: SequenceId,
    pub agent_turn: Option<crate::TurnId>,
    pub content: TranscriptContent,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum TranscriptInvalidation {
    None {},
    All {},
    Items { items: Vec<TranscriptItemId> },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum TranscriptAnchor {
    Unspecified {},
    Exact {
        item: TranscriptItemId,
    },
    Replaced {
        requested: TranscriptItemId,
        replacement: Option<TranscriptItemId>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptPage {
    pub view: TranscriptView,
    pub first_ordinal: TranscriptOrdinal,
    pub total_items: TranscriptOrdinal,
    pub items: Vec<TranscriptItem>,
    pub anchor: TranscriptAnchor,
    pub invalidation: TranscriptInvalidation,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum TranscriptReadResult {
    Ready {
        page: TranscriptPage,
    },
    CatchingUp {
        through: Option<SequenceId>,
        target: Option<SequenceId>,
    },
    OrderingChanged {
        view: TranscriptView,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[derive(Allocation)]
pub struct TranscriptContentRead {
    pub view: TranscriptView,
    pub source: TranscriptContentSource,
    pub offset: u32,
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptContentPage {
    pub view: TranscriptView,
    pub source: TranscriptContentSource,
    pub offset: u32,
    pub next_offset: Option<u32>,
    pub total_bytes: u32,
    pub format: TranscriptPreviewFormat,
    pub text: String,
}
