//! Session listing rows: identity, live control state, and recorded activity.
use super::shared::{ClientId, ModelAlias, SessionId, decimal_option_u64, decimal_u64};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One active or resumable session returned by the engine host.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionDescriptor {
    pub session_id: SessionId,
    /// Human-facing session title.
    pub title: String,
    pub workspace_name: String,
    pub model: ModelAlias,
    pub driver_client_id: Option<ClientId>,
    pub shell_active: bool,
    /// Recorded history; null until the session index has projected it.
    pub activity: Option<SessionActivity>,
}

/// What a session's recorded history holds, from the session index and the
/// accounting ledger.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionActivity {
    /// Last durable activity, in Unix milliseconds.
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub updated_unix_ms: u64,
    /// Accepted user turns.
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub turn_count: u64,
    /// Bounded, whitespace-collapsed preview of the first user prompt.
    pub first_prompt: Option<String>,
    /// Lifetime USD spend in micro-dollars; null when nothing was charged or
    /// some entries have no USD price.
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub cost_micros_usd: Option<u64>,
}
