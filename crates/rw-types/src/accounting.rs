//! Exact provider-attempt accounting facts, independent of turn display rollups.

use crate::{AccountingAttribution, Cost, SessionId, TurnId, Usage};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Host-assigned logical call identity plus a distinct provider attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallIdentity {
    /// Session which owns the provider request.
    pub session_id: SessionId,
    /// Immutable root session whose cap covers this session and its descendants.
    pub budget_session_id: SessionId,
    /// Durable agent turn which owns this request.
    pub turn_id: TurnId,
    /// Separates ordinary generation from compaction, title, and child usage.
    pub attribution: AccountingAttribution,
    /// Bounded host-generated identity, never a model-supplied tool identifier.
    pub call_id: String,
    /// Retries under a logical call must increment this value.
    pub attempt: u32,
}

/// Provider-reported actuals; a missing or ambiguous terminal is never represented as zero.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallActuals {
    /// Normalized input, output, cache and reasoning usage.
    pub usage: Usage,
    /// Normalized monetary, credit, subscription, or unavailable accounting.
    pub cost: Cost,
}
