//! Bounded search hits and exact journal-qualified transcript navigation.
use crate::{SequenceId, SessionDescriptor, SessionId};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A matching document at a complete, immutable search projection prefix.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchMatch {
    pub session_id: SessionId,
    pub source_sequence: SequenceId,
    pub through: SequenceId,
    pub digest: [u8; 32],
}

/// Session-wide term match; title-only matches have no transcript source.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchHit {
    pub session: SessionDescriptor,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SessionSearchMatch>")]
    #[ts(optional = false)]
    pub r#match: Option<SessionSearchMatch>,
}
