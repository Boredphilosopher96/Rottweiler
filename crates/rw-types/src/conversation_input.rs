//! The selected text of an accepted user input; attachments remain at its source.
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum InputSelection {
    Accepted {},
    Transformed { text: String },
}

/// Explicit sources for user-role context produced by the host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum ContextSelection {
    PlanReview {
        source: crate::SequenceId,
    },
    Retained {
        selected_source: crate::SequenceId,
        body_source: crate::SequenceId,
    },
    Continuation {},
}
