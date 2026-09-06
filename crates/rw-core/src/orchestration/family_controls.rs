//! Live family paths are resolved through retained owned sessions, never client IDs alone.
use super::{
    OrchestrationError, SessionRecord, SessionState, SubagentOrchestrator, SubagentSession,
};
use crate::engine::control_observation;
use rw_types::{
    SessionId,
    family_controls::{
        ChildControlHop, ChildControlTarget, FamilyControlRow, FamilyControlsSnapshot,
        MAX_FAMILY_CONTROL_DEPTH, MAX_FAMILY_CONTROL_ROWS, MAX_FAMILY_CONTROLS_BYTES,
        MAX_FAMILY_CONTROLS_PREPARED_BYTES,
    },
};
use std::{collections::HashMap, sync::Arc};

impl SubagentOrchestrator {
    /// # Errors
    /// Rejects inconsistent live bindings or a discovery result exceeding its source bound.
    pub async fn family_controls(
        &self,
        root: &SessionId,
        after: Option<rw_types::SequenceId>,
    ) -> Result<FamilyControlsSnapshot, OrchestrationError> {
        control_observation::wait(after).await;
        let revision = control_observation::revision();
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sessions.len() > MAX_FAMILY_CONTROL_ROWS {
            return Err(invalid("family control row admission"));
        }
        let by_session: HashMap<_, _> = sessions
            .values()
            .map(|record| (&record.handle.session_id, record))
            .collect();
        let mut children = Vec::new();
        for record in sessions.values() {
            if let Some(ancestry) = path(root, record, &by_session)? {
                children.push(FamilyControlRow {
                    target: ChildControlTarget {
                        ancestry,
                        session_id: record.handle.session_id.clone(),
                    },
                    controls: record.session.control_summary(),
                });
            }
        }
        children.sort_by(|left, right| {
            left.target
                .ancestry
                .iter()
                .map(|id| &id.subagent_id.0)
                .cmp(right.target.ancestry.iter().map(|id| &id.subagent_id.0))
        });
        let snapshot = FamilyControlsSnapshot { revision, children };
        if rw_types::allocation::PrepareAllocation::prepared_bytes(&snapshot)
            .is_none_or(|bytes| bytes > MAX_FAMILY_CONTROLS_PREPARED_BYTES)
        {
            return Err(invalid("family control prepared admission"));
        }
        rw_types::session_controls::encoded_size(&snapshot, MAX_FAMILY_CONTROLS_BYTES)
            .map_err(|_| invalid("family control encoded admission"))?;
        Ok(snapshot)
    }
    /// # Errors
    /// Rejects a path outside the root's actual ownership or a closing child.
    pub fn control_child(
        &self,
        root: &SessionId,
        target: &ChildControlTarget,
    ) -> Result<Arc<dyn SubagentSession>, OrchestrationError> {
        target.validate().map_err(invalid)?;
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut parent = root;
        let mut result = None;
        for id in &target.ancestry {
            let record = sessions
                .get(&id.subagent_id)
                .ok_or_else(|| invalid("unknown live child control path"))?;
            if record.handle.session_id != id.session_id
                || &record.parent_session_id != parent
                || matches!(record.state, SessionState::Closing)
            {
                return Err(invalid("child control path is not owned by this root"));
            }
            parent = &record.handle.session_id;
            result = Some(record.session.clone());
        }
        if parent != &target.session_id {
            return Err(invalid("child control session identity mismatch"));
        }
        result.ok_or_else(|| invalid("empty live child control path"))
    }
}
fn path(
    root: &SessionId,
    leaf: &SessionRecord,
    sessions: &HashMap<&SessionId, &SessionRecord>,
) -> Result<Option<Vec<ChildControlHop>>, OrchestrationError> {
    let mut record = leaf;
    let mut result = Vec::new();
    loop {
        if result.len() == MAX_FAMILY_CONTROL_DEPTH {
            return Err(invalid("family control ancestry depth"));
        }
        result.push(ChildControlHop {
            subagent_id: record.handle.subagent_id.clone(),
            session_id: record.handle.session_id.clone(),
        });
        if &record.parent_session_id == root {
            result.reverse();
            return Ok(Some(result));
        }
        let Some(parent) = sessions.get(&record.parent_session_id) else {
            return Ok(None);
        };
        record = parent;
    }
}
fn invalid(message: &str) -> OrchestrationError {
    OrchestrationError::InvalidRequest(message.into())
}
