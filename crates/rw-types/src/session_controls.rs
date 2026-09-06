//! Bounded live control state, independent of durable event replay position.
use crate::allocation::PrepareAllocation;
use crate::{
    PlanArtifact, Question, QuestionId, SequenceId, ToolCallId, ToolCapability, ToolInvocationId,
    TurnId, UnifiedDiff,
};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Write};
use ts_rs::TS;

pub const MAX_SESSION_CONTROLS_BYTES: usize = 7 * 1024 * 1024;
pub const MAX_SESSION_CONTROLS_PREPARED_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PENDING_PLAN_BYTES: usize = 256 * 1024;
pub const MAX_PENDING_PLAN_PREPARED_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionQuestion {
    pub question_id: QuestionId,
    pub turn_id: TurnId,
    pub question: Question,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionApproval {
    pub invocation_id: ToolInvocationId,
    pub tool_call_id: ToolCallId,
    pub turn_id: TurnId,
    pub name: String,
    pub args: Value,
    pub capabilities: Vec<ToolCapability>,
    pub rationale: String,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<UnifiedDiff>")]
    pub diff: Option<UnifiedDiff>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionControls {
    #[schemars(length(max = crate::question_admission::MAX_PENDING_QUESTION_REQUESTS))]
    pub questions: Vec<SessionQuestion>,
    #[schemars(length(max = crate::tool_admission::MAX_PENDING_TOOL_INVOCATIONS))]
    pub approvals: Vec<SessionApproval>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<PlanArtifact>")]
    pub pending_plan: Option<PlanArtifact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionControlsSnapshot {
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<SequenceId>")]
    pub through: Option<SequenceId>,
    pub controls: SessionControls,
}

/// # Errors
/// Checks a submitted plan before retaining or announcing it.
pub fn validate_plan(plan: &PlanArtifact) -> Result<(), &'static str> {
    if plan
        .prepared_bytes()
        .is_none_or(|bytes| bytes > MAX_PENDING_PLAN_PREPARED_BYTES)
    {
        return Err("plan prepared allocation exceeds admission");
    }
    encoded_size(plan, MAX_PENDING_PLAN_BYTES).map(|_| ())
}

/// Count serialized bytes without constructing an intermediate buffer.
/// # Errors
/// Rejects serialization failure or an over-limit payload.
pub fn encoded_size(value: &impl Serialize, limit: usize) -> Result<usize, &'static str> {
    let mut counter = LimitedSize { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| "control payload exceeds byte admission")?;
    Ok(counter.bytes)
}
struct LimitedSize {
    bytes: usize,
    limit: usize,
}
impl Write for LimitedSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|size| *size <= self.limit)
            .ok_or_else(|| io::Error::other("control byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_requires_nullable_fields_and_plan_admission_counts_escaping() {
        let valid = serde_json::json!({"through":null,"controls":{"questions":[],"approvals":[],"pending_plan":null}});
        assert!(serde_json::from_value::<SessionControlsSnapshot>(valid.clone()).is_ok());
        let mut missing = valid;
        missing.as_object_mut().expect("object").remove("through");
        assert!(serde_json::from_value::<SessionControlsSnapshot>(missing).is_err());
        let plan = PlanArtifact {
            title: "plan".into(),
            summary_md: "\0".repeat(MAX_PENDING_PLAN_BYTES / 2),
            steps: Vec::new(),
            open_questions: Vec::new(),
        };
        assert!(validate_plan(&plan).is_err());
    }
}
