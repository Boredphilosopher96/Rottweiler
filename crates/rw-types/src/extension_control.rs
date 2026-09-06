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
pub const MAX_CONTEXT_ITEM_ID_BYTES: usize =
    crate::tool_admission::MAX_TOOL_NAME_BYTES + "tool:".len();

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionContextRead {
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    pub expected_sequence: Option<SequenceId>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "nullable_context_item_schema")]
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
    ToolRegistry,
}

/// Metadata only. Prompt bodies, tool outputs and machine-local paths are not exposed.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionContextItem {
    #[schemars(schema_with = "context_item_schema")]
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
        #[schemars(schema_with = "nullable_context_item_schema")]
        next_after_item_id: Option<ContextItemId>,
    },
}

/// A request to the initiating client, resolved through its ordinary session/history APIs.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionNavigationTarget {
    Session {
        session_id: crate::SessionId,
    },
    /// Select the nearest surviving transcript row at or before this durable sequence.
    Transcript {
        sequence: SequenceId,
    },
}
impl SessionNavigationTarget {
    /// # Errors
    /// Rejects unsafe session identities. Transcript bounds are checked by the actor.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Session { session_id } => crate::SessionId::validate(&session_id.0)
                .map_err(|_| "invalid navigation session identity"),
            Self::Transcript { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionControl {
    /// Only an active driver command may request client navigation.
    Navigate { target: SessionNavigationTarget },
    PinContext {
        #[schemars(schema_with = "context_item_schema")]
        item_id: ContextItemId,
    },
    EvictContext {
        #[schemars(schema_with = "context_item_schema")]
        item_id: ContextItemId,
    },
    SelectMode {
        #[schemars(schema_with = "name_schema")]
        mode: ModeId,
    },
    SelectModel {
        #[schemars(schema_with = "name_schema")]
        model: ModelAlias,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "nullable_name_schema")]
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

/// Validate one bounded model, mode or provider identity.
/// # Errors
/// Rejects empty, oversized or control-bearing identities.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    validate_identity(name, MAX_CONTROL_NAME_BYTES)
}

/// Validate a context identity, including the tool namespace prefix.
/// # Errors
/// Rejects empty, oversized or control-bearing identities.
pub fn validate_context_item_id(name: &str) -> Result<(), &'static str> {
    validate_identity(name, MAX_CONTEXT_ITEM_ID_BYTES)
}

fn validate_identity(name: &str, limit: usize) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > limit || name.chars().any(char::is_control) {
        Err("control identity must be nonempty, bounded and free of control characters")
    } else {
        Ok(())
    }
}
impl ExtensionControl {
    /// Validate this explicit operation before actor admission.
    /// # Errors
    /// Rejects empty, oversized or control-bearing identities.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Navigate { target } => target.validate(),
            Self::PinContext { item_id } | Self::EvictContext { item_id } => {
                validate_context_item_id(&item_id.0)
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

fn name_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    identity_schema(MAX_CONTROL_NAME_BYTES)
}
fn context_item_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    identity_schema(MAX_CONTEXT_ITEM_ID_BYTES)
}
#[allow(clippy::expect_used)]
fn identity_schema(limit: usize) -> schemars::Schema {
    serde_json::json!({"type":"string", "minLength":1, "maxLength":limit,
        "x-rw-max-utf8-bytes":limit,
        "pattern":r"^[^\u0000-\u001f\u007f-\u009f]+$"})
    .try_into()
    .expect("name schema")
}
#[allow(clippy::expect_used)]
fn nullable_name_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::json!({"anyOf":[name_schema(generator), {"type":"null"}]})
        .try_into()
        .expect("nullable name schema")
}

#[allow(clippy::expect_used)]
fn nullable_context_item_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::json!({"anyOf":[context_item_schema(generator), {"type":"null"}]})
        .try_into()
        .expect("nullable context item schema")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_identity_includes_the_full_admitted_tool_name() {
        let id = format!(
            "tool:{}",
            "x".repeat(crate::tool_admission::MAX_TOOL_NAME_BYTES)
        );
        assert!(validate_context_item_id(&id).is_ok());
        assert!(validate_context_item_id(&format!("{id}x")).is_err());
        assert!(validate_name(&id).is_err());
        assert!(validate_context_item_id("tool:x\n").is_err());
        assert!(validate_context_item_id(&"é".repeat(MAX_CONTEXT_ITEM_ID_BYTES / 2 + 1)).is_err());
    }
}
