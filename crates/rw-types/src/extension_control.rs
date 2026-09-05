//! Typed, session-bound extension controls and bounded context inventory.
use crate::{
    ContextItemId, ContextItemKind, ContextItemState, ModeId, ModelAlias, QuestionId, SequenceId,
};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_CONTEXT_PAGE_ITEMS: usize = 128;
pub const MAX_CONTROL_NAME_BYTES: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionContextRead {
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    pub expected_sequence: Option<SequenceId>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<ContextItemId>")]
    pub after_item_id: Option<ContextItemId>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionContextSource {
    BuiltIn,
    ProjectFile,
    Extension,
    Conversation,
    UserPin,
    ClientQueue,
}

/// Metadata only. Prompt bodies, tool outputs and machine-local paths are not exposed.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionContextItem {
    pub item_id: ContextItemId,
    pub kind: ContextItemKind,
    pub source: ExtensionContextSource,
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub estimated_tokens: u64,
    pub state: ContextItemState,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionContextPage {
    Restart {},
    Ready {
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
        sequence: Option<SequenceId>,
        #[schemars(length(max = 128))]
        items: Vec<ExtensionContextItem>,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<ContextItemId>")]
        next_after_item_id: Option<ContextItemId>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionControl {
    PinContext {
        item_id: ContextItemId,
    },
    EvictContext {
        item_id: ContextItemId,
    },
    SelectMode {
        mode: ModeId,
    },
    SelectModel {
        model: ModelAlias,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
        provider: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionControlOutcome {
    Applied {},
    Busy {},
    ContextChoiceRequired { question_id: QuestionId },
}

pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > MAX_CONTROL_NAME_BYTES || name.chars().any(char::is_control)
    {
        Err("control identity must be nonempty, bounded and free of control characters")
    } else {
        Ok(())
    }
}
impl ExtensionControl {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::PinContext { item_id } | Self::EvictContext { item_id } => {
                validate_name(&item_id.0)
            }
            Self::SelectMode { mode } => validate_name(&mode.0),
            Self::SelectModel { model, provider } => {
                validate_name(&model.0)?;
                provider
                    .as_deref()
                    .map(validate_name)
                    .transpose()
                    .map(|_| ())
            }
        }
    }
}
