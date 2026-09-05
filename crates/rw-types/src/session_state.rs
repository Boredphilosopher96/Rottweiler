//! Bounded live session metadata at one actor dispatch boundary.
use crate::{
    BudgetLevel, BudgetScope, BudgetUnit, ClientId, ModeId, ModelAlias, SequenceId, ShellId,
    TurnId, config::ThinkingLevel,
};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_SESSION_STATE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SESSION_STATE_PREPARED_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SESSION_QUEUE_PREVIEW_BYTES: usize = 1024;
pub const MAX_SESSION_QUEUE_ITEMS: usize = 128;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionStateSnapshot {
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    pub through: Option<SequenceId>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<ClientId>")]
    pub driver_client_id: Option<ClientId>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    pub title: Option<String>,
    pub model_alias: ModelAlias,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    pub provider: Option<String>,
    pub thinking: ThinkingLevel,
    pub mode_id: ModeId,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SessionActiveTurn>")]
    pub active_turn: Option<SessionActiveTurn>,
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub completed_turns: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SessionShellState>")]
    pub shell: Option<SessionShellState>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SessionCompactionState>")]
    pub compaction: Option<SessionCompactionState>,
    #[schemars(length(max = MAX_SESSION_QUEUE_ITEMS))]
    pub queued_messages: Vec<SessionQueuedPreview>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SessionBudgetState>")]
    pub budget: Option<SessionBudgetState>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionActiveTurn {
    pub turn_id: TurnId,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    pub started: Option<SequenceId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionShellState {
    pub shell_id: ShellId,
    #[schemars(length(max = MAX_SESSION_QUEUE_PREVIEW_BYTES), extend("x-rw-max-utf8-bytes" = MAX_SESSION_QUEUE_PREVIEW_BYTES))]
    pub command_preview: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactionState {
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub revision: u64,
    pub text: crate::transcript_tail::TranscriptTailText,
    pub thinking: crate::transcript_tail::TranscriptTailText,
    pub summary_turn_id: TurnId,
    pub started: SequenceId,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<u32>")]
    pub attempt: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionQueuedPreview {
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub position: u64,
    #[schemars(length(max = MAX_SESSION_QUEUE_PREVIEW_BYTES), extend("x-rw-max-utf8-bytes" = MAX_SESSION_QUEUE_PREVIEW_BYTES))]
    pub preview: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionBudgetState {
    pub turn_id: TurnId,
    pub level: BudgetLevel,
    pub scope: BudgetScope,
    pub unit: BudgetUnit,
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub current: u64,
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub limit: u64,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{SessionActiveTurn, SessionQueuedPreview};
    use crate::{SequenceId, TurnId};

    #[test]
    fn active_source_is_required_nullable_and_exact() {
        let value = SessionActiveTurn {
            turn_id: TurnId("turn-1".into()),
            started: Some(SequenceId(u64::MAX)),
        };
        let encoded = serde_json::to_value(&value).expect("encode");
        assert_eq!(encoded["started"], u64::MAX.to_string());
        assert_eq!(
            serde_json::from_value::<SessionActiveTurn>(encoded).expect("decode"),
            value
        );
        assert!(
            serde_json::from_value::<SessionActiveTurn>(serde_json::json!({"turn_id":"turn-1"}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<SessionActiveTurn>(
                serde_json::json!({"turn_id":"turn-1","started":null})
            )
            .is_ok()
        );
    }

    #[test]
    fn queue_positions_do_not_round_at_client_integer_precision() {
        let value = SessionQueuedPreview {
            position: u64::MAX,
            preview: "queued".into(),
            truncated: false,
        };
        let encoded = serde_json::to_value(&value).expect("encode");
        assert_eq!(encoded["position"], u64::MAX.to_string());
        assert_eq!(
            serde_json::from_value::<SessionQueuedPreview>(encoded).expect("decode"),
            value
        );
    }
}
