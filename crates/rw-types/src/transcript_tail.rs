//! Source-owned limits for bounded in-progress transcript previews.

/// Text and reasoning each retain a UTF-8 prefix; committed IR owns full content.
pub const TRANSCRIPT_TAIL_TEXT_BYTES: usize = 64 * 1024;
/// Each admitted invocation retains a combined-stream display prefix.
pub const TRANSCRIPT_TAIL_TOOL_BYTES: usize = 8 * 1024;

use crate::citation_admission::MAX_CITATION_TEXT_BYTES;
use crate::transcript::{TranscriptBodyPreview, TranscriptGeneration, TranscriptView};
use crate::{SequenceId, ToolCallId, ToolInvocationId, TurnId};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const TRANSCRIPT_TAIL_PAGE_BYTES: usize = 1024 * 1024;
pub const TRANSCRIPT_TAIL_MIN_PAGE_BYTES: usize = 512 * 1024;
pub const TRANSCRIPT_TAIL_PAGE_ITEMS: usize = 32;

/// Structural and response identity; pages can advance their source cut within this identity.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptTailIdentity {
    pub generation: TranscriptGeneration,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    #[ts(optional = false)]
    pub turn_started: Option<SequenceId>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    #[ts(optional = false)]
    pub response_epoch: Option<SequenceId>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    #[ts(optional = false)]
    pub tools_epoch: Option<SequenceId>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum TranscriptTailPart {
    Text {},
    Thinking {},
    Citations { offset: u16 },
    Tools { offset: u16 },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptTailRead {
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<TranscriptTailIdentity>")]
    #[ts(optional = false)]
    pub expected: Option<TranscriptTailIdentity>,
    pub part: TranscriptTailPart,
    #[schemars(range(min = 1, max = TRANSCRIPT_TAIL_PAGE_ITEMS))]
    pub max_items: u16,
    #[schemars(range(min = TRANSCRIPT_TAIL_MIN_PAGE_BYTES, max = TRANSCRIPT_TAIL_PAGE_BYTES))]
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptTailText {
    #[schemars(length(max = TRANSCRIPT_TAIL_TEXT_BYTES), extend("x-rw-max-utf8-bytes" = TRANSCRIPT_TAIL_TEXT_BYTES))]
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptTailCitation {
    pub source: SequenceId,
    #[schemars(length(max = MAX_CITATION_TEXT_BYTES), extend("x-rw-max-utf8-bytes" = MAX_CITATION_TEXT_BYTES))]
    pub uri: String,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>", length(max = MAX_CITATION_TEXT_BYTES), extend("x-rw-max-utf8-bytes" = MAX_CITATION_TEXT_BYTES))]
    #[ts(optional = false)]
    pub title: Option<String>,
}

/// Active invocation preview. Argument and diff bodies remain canonical content sources.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TranscriptTailTool {
    pub source: SequenceId,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub invocation_id: ToolInvocationId,
    pub name: String,
    pub call_index: u32,
    pub arguments: TranscriptBodyPreview,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<TranscriptBodyPreview>")]
    #[ts(optional = false)]
    pub diff: Option<TranscriptBodyPreview>,
    pub output: TranscriptTailText,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum TranscriptTailContent {
    Text {
        preview: TranscriptTailText,
    },
    Thinking {
        preview: TranscriptTailText,
    },
    Citations {
        offset: u16,
        #[schemars(length(max = TRANSCRIPT_TAIL_PAGE_ITEMS))]
        items: Vec<TranscriptTailCitation>,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<u16>")]
        #[ts(optional = false)]
        next_offset: Option<u16>,
    },
    Tools {
        offset: u16,
        #[schemars(length(max = TRANSCRIPT_TAIL_PAGE_ITEMS))]
        items: Vec<TranscriptTailTool>,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<u16>")]
        #[ts(optional = false)]
        next_offset: Option<u16>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-rw-max-json-bytes" = TRANSCRIPT_TAIL_PAGE_BYTES))]
pub struct TranscriptTailPage {
    pub view: TranscriptView,
    pub identity: TranscriptTailIdentity,
    pub content: TranscriptTailContent,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum TranscriptTailResult {
    Ready {
        page: TranscriptTailPage,
    },
    Changed {
        view: TranscriptView,
        identity: TranscriptTailIdentity,
    },
    CatchingUp {
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
        #[ts(optional = false)]
        through: Option<SequenceId>,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
        #[ts(optional = false)]
        target: Option<SequenceId>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[allow(clippy::expect_used)]
    fn tail_requests_require_nullable_fence_and_closed_part_shapes() {
        let value = json!({"expected":null,"part":{"type":"text"},"max_items":1,"max_bytes":TRANSCRIPT_TAIL_MIN_PAGE_BYTES});
        assert!(serde_json::from_value::<TranscriptTailRead>(value.clone()).is_ok());
        let mut omitted = value.clone();
        omitted.as_object_mut().expect("object").remove("expected");
        assert!(serde_json::from_value::<TranscriptTailRead>(omitted).is_err());
        let mut extra = value;
        extra["part"]["offset"] = json!(0);
        assert!(serde_json::from_value::<TranscriptTailRead>(extra).is_err());
        let schema = schemars::schema_for!(TranscriptTailRead);
        assert_eq!(
            schema["properties"]["max_bytes"]["maximum"],
            json!(TRANSCRIPT_TAIL_PAGE_BYTES)
        );
        assert_eq!(
            schema["properties"]["max_items"]["maximum"],
            json!(TRANSCRIPT_TAIL_PAGE_ITEMS)
        );
    }
}
