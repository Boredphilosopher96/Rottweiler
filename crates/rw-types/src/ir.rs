use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS, Allocation,
        )]
        pub struct $name(pub String);
    };
}

string_id!(
    ToolCallId,
    "Stable identifier assigned to a model tool call."
);

/// Host-owned identity of one tool execution, independent of provider call IDs.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    JsonSchema,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    TS,
    Allocation,
)]
pub struct ToolInvocationId(pub String);

/// A provider-neutral role in the conversation IR.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// An image reference that does not assume a filesystem shared with a client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
pub enum ImageRef {
    InlineBase64 { data: String },
    Url { url: String },
}

/// One part of a mixed tool result.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
pub enum ToolOutputPart {
    Text { text: String },
    Structured { value: Value },
    Image { media_type: String, data: ImageRef },
}

/// Provider-neutral output produced by a tool.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
pub enum ToolOutput {
    Text { text: String },
    Structured { value: Value },
    Mixed { parts: Vec<ToolOutputPart> },
}

/// A provider-neutral content block.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case", optional_fields = nullable)]
#[derive(Allocation)]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        content: String,
        signature: Option<String>,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        args: Value,
    },
    ToolResult {
        id: ToolCallId,
        output: ToolOutput,
        is_error: bool,
    },
    Image {
        media_type: String,
        data: ImageRef,
    },
    Citation {
        uri: String,
        title: Option<String>,
        excerpt: Option<String>,
    },
}

/// Metadata whose meaning is independent of a provider adapter.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(default)]
#[ts(optional_fields = nullable)]
#[derive(Allocation)]
pub struct TurnMeta {
    pub created_at: Option<String>,
    pub model: Option<String>,
    pub synthetic: bool,
    pub summary: bool,
}

/// One conversation turn in the internal message representation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
pub struct Turn {
    pub role: Role,
    pub blocks: Vec<Block>,
    pub meta: TurnMeta,
}
