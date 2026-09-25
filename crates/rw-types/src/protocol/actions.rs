//! Bounded actor-owned availability for session controls.
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Controls whose readiness is checked by the engine and projected to clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SessionActionKind {
    SwitchModel,
    SwitchMode,
    Compact,
    Rewind,
    Review,
    Fork,
    AddWorkspaceRoot,
    MutateContext,
}

/// Availability is advisory; the engine rechecks the action on submission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionActionAvailability {
    pub action: SessionActionKind,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[ts(as = "Option<_>", optional)]
    pub queued: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>", extend("maxLength" = 256))]
    pub unavailable_reason: Option<String>,
}

pub const MAX_QUEUED_SESSION_CONTROLS: usize = 8;
pub const MAX_CONTROL_INSTRUCTIONS_BYTES: usize = 4096;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum DeferredSessionAction {
    SwitchModel {
        model: super::ModelAlias,
        provider: Option<String>,
    },
    SwitchMode {
        mode: super::ModeId,
    },
    Compact {
        instructions: Option<String>,
    },
}
impl DeferredSessionAction {
    #[must_use]
    pub const fn kind(&self) -> SessionActionKind {
        match self {
            Self::SwitchModel { .. } => SessionActionKind::SwitchModel,
            Self::SwitchMode { .. } => SessionActionKind::SwitchMode,
            Self::Compact { .. } => SessionActionKind::Compact,
        }
    }
    /// # Errors
    /// Rejects unbounded control payloads before queue ownership is acquired.
    pub fn validate(&self) -> Result<(), &'static str> {
        let bounded = |value: &str| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        };
        match self {
            Self::SwitchModel { model, provider }
                if !bounded(&model.0) || provider.as_ref().is_some_and(|value| !bounded(value)) =>
            {
                Err("model selection exceeds its control limit")
            }
            Self::SwitchMode { mode } if !bounded(&mode.0) => {
                Err("mode selection exceeds its control limit")
            }
            Self::Compact {
                instructions: Some(value),
            } if value.len() > MAX_CONTROL_INSTRUCTIONS_BYTES => {
                Err("compaction instructions exceed their control limit")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum QueuedControlStatus {
    Queued,
    Running,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct QueuedSessionControl {
    pub request: super::CommandMeta,
    pub action: DeferredSessionAction,
    pub status: QueuedControlStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SessionControlOutcome {
    Applied,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionControlSettlement {
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cancelled_question: Option<super::QuestionId>,
    pub request: super::CommandMeta,
    pub action: SessionActionKind,
    pub outcome: SessionControlOutcome,
    pub message: String,
}

/// # Errors
/// Rejects oversized queues, duplicate identities, or malformed owned payloads.
pub fn validate_queued_controls(controls: &[QueuedSessionControl]) -> Result<(), &'static str> {
    if controls.len() > MAX_QUEUED_SESSION_CONTROLS {
        return Err("session control queue is full");
    }
    for (index, control) in controls.iter().enumerate() {
        if control.request.protocol_version != crate::PROTOCOL_VERSION
            || !control.request.request_id.is_valid()
            || control.request.client_id.0.is_empty()
            || control.request.client_id.0.len() > 256
            || (control.status == QueuedControlStatus::Running && index != 0)
            || controls[..index].iter().any(|earlier| {
                earlier.request.client_id == control.request.client_id
                    && earlier.request.request_id == control.request.request_id
            })
        {
            return Err("invalid queued control identity or ordering");
        }
        control.action.validate()?;
    }
    Ok(())
}
