//! Live descendant control capabilities and bounded discovery, independent of progress.
use crate::{
    Answer, ApprovalBinding, ApprovalDecision, PlanDecision, QuestionId, SequenceId, SessionId,
    SubagentId, ToolCallId, ToolInvocationId,
};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_FAMILY_CONTROL_WAITS: usize = 64;
pub const MAX_CLIENT_FAMILY_CONTROL_WAITS: usize = 1;
pub const FAMILY_CONTROL_WAIT_MILLIS: usize = 10_000;
pub const MAX_FAMILY_CONTROL_DEPTH: usize = 8;
pub const MAX_FAMILY_CONTROL_ROWS: usize = crate::session_children::MAX_ACTIVE_CHILDREN;
pub const MAX_FAMILY_CONTROLS_BYTES: usize = 512 * 1024;
pub const MAX_FAMILY_CONTROLS_PREPARED_BYTES: usize = 2 * 1024 * 1024;

/// An exact path through the live parent's owned child bindings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ChildControlTarget {
    #[schemars(length(min = 1, max = MAX_FAMILY_CONTROL_DEPTH))]
    pub ancestry: Vec<ChildControlHop>,
    pub session_id: SessionId,
}
impl ChildControlTarget {
    /// # Errors
    /// Rejects empty, cyclic, over-depth paths and invalid child identities.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.ancestry.is_empty() || self.ancestry.len() > MAX_FAMILY_CONTROL_DEPTH {
            return Err("child control ancestry depth");
        }
        SessionId::validate(&self.session_id.0).map_err(|_| "child session identity")?;
        if self
            .ancestry
            .last()
            .is_none_or(|hop| hop.session_id != self.session_id)
        {
            return Err("child control target terminal identity");
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in &self.ancestry {
            SessionId::validate(&id.session_id.0).map_err(|_| "child ancestry session identity")?;
            if id.subagent_id.0.is_empty() || !seen.insert(&id.session_id.0) {
                return Err("child control ancestry identity");
            }
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ChildControlTarget {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            ancestry: Vec<ChildControlHop>,
            session_id: SessionId,
        }
        let wire = Wire::deserialize(decoder)?;
        let value = Self {
            ancestry: wire.ancestry,
            session_id: wire.session_id,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

/// Revision is a live control fence, not a journal cursor. `through` is the durable cut.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ChildControlSummary {
    pub revision: SequenceId,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    pub through: Option<SequenceId>,
    #[schemars(range(max = crate::question_admission::MAX_PENDING_QUESTION_REQUESTS))]
    pub questions: u32,
    #[schemars(range(max = crate::tool_admission::MAX_PENDING_TOOL_INVOCATIONS))]
    pub approvals: u32,
    pub pending_plan: bool,
    pub available: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct FamilyControlRow {
    pub target: ChildControlTarget,
    pub controls: ChildControlSummary,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-rw-max-json-bytes" = MAX_FAMILY_CONTROLS_BYTES))]
pub struct FamilyControlsSnapshot {
    /// Reconnect starts with an unconditional read; this fence belongs to the live host.
    pub revision: SequenceId,
    #[schemars(length(max = MAX_FAMILY_CONTROL_ROWS))]
    pub children: Vec<FamilyControlRow>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ChildControlsSnapshot {
    pub revision: SequenceId,
    pub snapshot: crate::session_controls::SessionControlsSnapshot,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum ChildControlResponse {
    Question {
        question_id: QuestionId,
        answers: Vec<Answer>,
    },
    Approval {
        tool_call_id: ToolCallId,
        invocation_id: ToolInvocationId,
        decision: ApprovalDecision,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<ApprovalBinding>")]
        binding: Option<ApprovalBinding>,
    },
    Plan {
        decision: PlanDecision,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
        revisions: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ChildControlHop {
    pub subagent_id: SubagentId,
    pub session_id: SessionId,
}
impl Default for ChildControlSummary {
    fn default() -> Self {
        Self {
            revision: SequenceId(0),
            through: None,
            questions: 0,
            approvals: 0,
            pending_plan: false,
            available: false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{ChildControlResponse, ChildControlTarget};
    #[test]
    fn child_control_target_requires_exact_bounded_ancestry() {
        let hop = serde_json::json!({"subagent_id":"agent-1","session_id":"child-1"});
        let valid = serde_json::json!({"ancestry":[hop.clone()],"session_id":"child-1"});
        assert!(serde_json::from_value::<ChildControlTarget>(valid.clone()).is_ok());
        for ancestry in [
            serde_json::json!([]),
            serde_json::json!([hop.clone(), hop.clone()]),
            serde_json::json!(vec![hop; 9]),
        ] {
            let mut invalid = valid.clone();
            invalid["ancestry"] = ancestry;
            assert!(serde_json::from_value::<ChildControlTarget>(invalid).is_err());
        }
        assert!(
            serde_json::from_value::<ChildControlResponse>(
                serde_json::json!({"type":"plan","decision":"approve"})
            )
            .is_err()
        );
    }
}
