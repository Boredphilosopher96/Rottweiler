use serde::{Deserialize, Serialize, Serializer, ser::Error as _};
use tempfile::tempdir;

use rw_types::{AccountingAttribution, Cost, SequenceId, TurnId, Usage};

use super::{
    AccountingLedger, EVENT_READ_HOOK, EventEnvelope, MAX_SEARCH_INDEX_BYTES,
    MAX_SEARCH_INDEX_WAL_BYTES, ProjectionStatus, SessionEventLog, SessionEventPageLimits,
    SessionIndex, SessionProjection, SessionStoreError, SessionSummary, TurnAccountingEntry,
    UtcDayKey, UtcTimestamp, garbage_collect_empty_sessions, install_append_fault, journal,
    upsert_projection,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct FixtureEvent {
    kind: String,
    text: String,
}

impl rw_types::allocation::DecodeAllocation for FixtureEvent {
    fn decode_node_bytes() -> Option<usize> {
        Some(std::mem::size_of::<Self>().max(std::mem::size_of::<String>()))
    }
}

fn accounting_entry(
    session_id: &str,
    turn: u64,
    sequence: u64,
    emitted_at_utc: &str,
    cost: Cost,
) -> TurnAccountingEntry {
    let emitted_at_utc = UtcTimestamp::parse(emitted_at_utc)
        .unwrap_or_else(|error| panic!("fixture timestamp must parse: {error}"));
    TurnAccountingEntry {
        session_id: session_id.to_owned(),
        turn_id: TurnId(turn.to_string()),
        sequence_id: SequenceId(sequence),
        utc_day: emitted_at_utc.utc_day(),
        emitted_at_utc,
        attribution: AccountingAttribution::Main,
        usage: Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost,
    }
}

fn utc_day(value: &str) -> UtcDayKey {
    UtcDayKey::parse(value).unwrap_or_else(|error| panic!("fixture day must parse: {error}"))
}

fn utc_timestamp(value: &str) -> UtcTimestamp {
    UtcTimestamp::parse(value)
        .unwrap_or_else(|error| panic!("fixture timestamp must parse: {error}"))
}

struct FailableEvent {
    text: &'static str,
    fail: bool,
}

impl rw_types::allocation::DecodeAllocation for FailableEvent {
    fn decode_node_bytes() -> Option<usize> {
        // The successful serializer emits exactly one JSON string.
        <String as rw_types::allocation::DecodeAllocation>::decode_node_bytes()
    }
}

impl Serialize for FailableEvent {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        if self.fail {
            Err(SerializerType::Error::custom(
                "fixture serialization failure",
            ))
        } else {
            serializer.serialize_str(self.text)
        }
    }
}

use std::{fs::OpenOptions, io::Write};

mod accounting;

mod index;

mod journal_append;

mod journal_history;
