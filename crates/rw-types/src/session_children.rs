//! Bounded active child associations at an exact canonical source prefix.
use crate::{SequenceId, SessionId, SubagentId};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_ACTIVE_CHILDREN: usize = 256;
pub const MAX_CHILD_TASK_PREVIEW_BYTES: usize = 1024;
pub const MAX_SESSION_CHILDREN_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SESSION_CHILDREN_PREPARED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-rw-max-json-bytes" = MAX_SESSION_CHILDREN_BYTES,
    "x-rw-item-budget" = serde_json::json!({"array":"children","identity":"subagent_id","fields":[],"maxUtf8Bytes":0})))]
pub struct SessionChildrenSnapshot {
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    pub through: Option<SequenceId>,
    #[schemars(length(max = MAX_ACTIVE_CHILDREN))]
    pub children: Vec<SessionChildState>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionChildState {
    pub subagent_id: SubagentId,
    pub child_session_id: SessionId,
    pub spawned: SequenceId,
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub spawned_turn: u64,
    #[schemars(length(max = MAX_CHILD_TASK_PREVIEW_BYTES), extend("x-rw-max-utf8-bytes" = MAX_CHILD_TASK_PREVIEW_BYTES))]
    pub task_preview: String,
    pub task_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum SessionChildrenResult {
    Ready {
        snapshot: SessionChildrenSnapshot,
    },
    CatchingUp {
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
        through: Option<SequenceId>,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
        target: Option<SequenceId>,
    },
}
impl SessionChildrenSnapshot {
    /// # Errors
    /// Rejects excess allocation, repeated identities and future association sources.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.children.len() > MAX_ACTIVE_CHILDREN {
            return Err("active child count");
        }
        let mut identities = std::collections::HashSet::new();
        for child in &self.children {
            if !identities.insert(&child.subagent_id) {
                return Err("duplicate active child");
            }
            if self.through.is_none_or(|through| child.spawned > through) {
                return Err("future child source");
            }
            if child.task_preview.len() > MAX_CHILD_TASK_PREVIEW_BYTES {
                return Err("child task preview");
            }
        }
        if crate::allocation::PrepareAllocation::prepared_bytes(self)
            .is_none_or(|bytes| bytes > MAX_SESSION_CHILDREN_PREPARED_BYTES)
        {
            return Err("active child prepared bytes");
        }
        crate::session_controls::encoded_size(self, MAX_SESSION_CHILDREN_BYTES)
            .map_err(|_| "active child snapshot bytes")?;
        Ok(())
    }
}
impl<'de> Deserialize<'de> for SessionChildrenSnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(deserialize_with = "Option::deserialize")]
            through: Option<SequenceId>,
            children: Vec<SessionChildState>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            through: wire.through,
            children: wire.children,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    fn child() -> SessionChildState {
        SessionChildState {
            subagent_id: SubagentId("agent".into()),
            child_session_id: SessionId("child".into()),
            spawned: SequenceId(5),
            spawned_turn: u64::MAX,
            task_preview: "task".into(),
            task_truncated: false,
        }
    }
    #[test]
    fn child_snapshot_requires_exact_sources_and_unique_bounded_associations() {
        let snapshot = SessionChildrenSnapshot {
            through: Some(SequenceId(5)),
            children: vec![child()],
        };
        let mut wire = serde_json::to_value(&snapshot).expect("encode");
        assert_eq!(wire["children"][0]["spawned_turn"], u64::MAX.to_string());
        assert_eq!(
            serde_json::from_value::<SessionChildrenSnapshot>(wire.clone()).expect("decode"),
            snapshot
        );
        wire.as_object_mut().expect("object").remove("through");
        assert!(serde_json::from_value::<SessionChildrenSnapshot>(wire).is_err());
        for invalid in [
            SessionChildrenSnapshot {
                through: None,
                children: vec![child()],
            },
            SessionChildrenSnapshot {
                through: Some(SequenceId(4)),
                children: vec![child()],
            },
            SessionChildrenSnapshot {
                through: Some(SequenceId(5)),
                children: vec![child(), child()],
            },
            SessionChildrenSnapshot {
                through: Some(SequenceId(5)),
                children: vec![SessionChildState {
                    task_preview: "€".repeat(MAX_CHILD_TASK_PREVIEW_BYTES),
                    ..child()
                }],
            },
        ] {
            assert!(invalid.validate().is_err());
            assert!(
                serde_json::from_value::<SessionChildrenSnapshot>(
                    serde_json::to_value(invalid).expect("encode invalid")
                )
                .is_err()
            );
        }
    }
}
