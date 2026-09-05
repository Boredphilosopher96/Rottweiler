use super::{
    ARTIFACT_IDENTITIES, ARTIFACTS, ArtifactIdentity, Head, IDENTITIES, PENDING, RAW_SPAWNS,
    STATES, SubagentBinding, VERSIONS, identity, key, raw_identity, reduce,
};
use crate::engine::recovery::{RecoveryError, read::SourceReader};
use rw_store::session::{journal::JournalReadView, recovery_index::RecoveryReadView};
use rw_types::{DiffArtifact, EngineEvent, SequenceId, SessionId, SubagentId};
use std::collections::VecDeque;

/// Immutable metadata transaction plus its exact canonical source prefix.
pub struct SubagentLifecycleView {
    pub(super) read: RecoveryReadView,
    pub(super) source: JournalReadView,
    pub(super) head: Head,
}
impl SubagentLifecycleView {
    #[must_use]
    pub fn through(&self) -> Option<SequenceId> {
        self.head.next.checked_sub(1).map(SequenceId)
    }
    /// Returns physical publication proof, even when its logical turn was rewound.
    /// # Errors
    /// Rejects malformed identities and corrupt metadata.
    pub fn published(
        &self,
        subagent: &SubagentId,
        session: &SessionId,
    ) -> Result<Option<SequenceId>, RecoveryError> {
        self.read
            .lookup(RAW_SPAWNS, &raw_identity(subagent, session)?)?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(RecoveryError::from))
            .transpose()
    }
    /// # Errors
    /// Resolves only the currently effective lifecycle for this exact child id.
    pub fn binding(&self, subagent: &SubagentId) -> Result<Option<SubagentBinding>, RecoveryError> {
        let Some(scope) = self.read.lookup(IDENTITIES, identity(subagent)?)? else {
            return Ok(None);
        };
        let scope = serde_json::from_slice(&scope)?;
        let result = self
            .read
            .get(key(STATES, 0, scope))?
            .map(|row| {
                serde_json::from_slice::<SubagentBinding>(&row.payload).map_err(RecoveryError::from)
            })
            .transpose()?;
        if result
            .as_ref()
            .is_some_and(|result| &result.subagent_id != subagent || result.scope != scope)
        {
            return Err(RecoveryError::Invalid("child binding identity"));
        }
        Ok(result)
    }
    /// Read at most32 unresolved spawns in deterministic publication order.
    /// # Errors
    /// Rejects invalid page limits or stale derived state.
    pub fn pending(
        &self,
        after: Option<SequenceId>,
        limit: usize,
    ) -> Result<Vec<SubagentBinding>, RecoveryError> {
        if limit == 0 || limit > 32 {
            return Err(RecoveryError::Limit("child recovery page"));
        }
        let page = self.read.page(
            PENDING,
            0,
            after.map(|sequence| sequence.0),
            limit,
            64 * 1024,
        )?;
        page.rows
            .into_iter()
            .map(|row| {
                let scope = serde_json::from_slice(&row.payload)?;
                let row_state = self
                    .read
                    .get(key(STATES, 0, scope))?
                    .ok_or(RecoveryError::Invalid("pending child state missing"))?;
                let state: SubagentBinding = serde_json::from_slice(&row_state.payload)?;
                if state.spawned.0 != row.key.ordinal
                    || state.terminal.is_some()
                    || state.scope != scope
                {
                    return Err(RecoveryError::Invalid("pending child identity"));
                }
                Ok(state)
            })
            .collect()
    }
    /// Read every active association from this immutable lifecycle generation.
    /// The producer admits at most 256 active children; a corrupt excess is rejected.
    /// # Errors
    /// Rejects inconsistent source selectors or an excessive active set.
    pub fn active_children(&self) -> Result<Vec<SubagentBinding>, RecoveryError> {
        let mut children = Vec::new();
        let mut after = None;
        loop {
            let page = self.pending(after, 32)?;
            if page.is_empty() {
                return Ok(children);
            }
            if children.len() + page.len() > rw_types::session_children::MAX_ACTIVE_CHILDREN {
                return Err(RecoveryError::Limit("active child snapshot"));
            }
            if page.iter().any(|child| {
                child.task_preview.len() > rw_types::session_children::MAX_CHILD_TASK_PREVIEW_BYTES
            }) {
                return Err(RecoveryError::Invalid("child task preview"));
            }
            after = page.last().map(|child| child.spawned);
            children.extend(page);
        }
    }
    /// Resolve one authorized artifact from its exact effective terminal source.
    /// # Errors
    /// Rejects mismatched selectors, artifact content identities and source corruption.
    pub fn artifact(&self, id: &str) -> Result<Option<DiffArtifact>, RecoveryError> {
        if id.is_empty() || id.len() > 256 {
            return Err(RecoveryError::Invalid("artifact identity"));
        }
        let Some(bytes) = self.read.lookup(ARTIFACT_IDENTITIES, id.as_bytes())? else {
            return Ok(None);
        };
        let identity: ArtifactIdentity = serde_json::from_slice(&bytes)?;
        let Some(row) = self
            .read
            .last_before(ARTIFACTS, identity.scope, self.head.next)?
        else {
            return Ok(None);
        };
        let scope: u64 = serde_json::from_slice(&row.payload)?;
        let state = self
            .read
            .get(key(VERSIONS, scope, row.key.ordinal))?
            .ok_or(RecoveryError::Invalid("artifact terminal revision missing"))?;
        let state: SubagentBinding = serde_json::from_slice(&state.payload)?;
        let event = SourceReader {
            source: &self.source,
            events: VecDeque::new(),
        }
        .event(SequenceId(row.key.ordinal))?;
        let EngineEvent::SubagentFinished {
            subagent_id,
            result,
            ..
        } = event
        else {
            return Err(RecoveryError::Invalid("artifact terminal source"));
        };
        if state.terminal != Some(SequenceId(row.key.ordinal))
            || subagent_id != state.subagent_id
            || result.session_id != state.session_id
            || result.subagent_id != subagent_id
        {
            return Err(RecoveryError::Invalid("artifact source identity"));
        }
        let artifact = result
            .diff_artifact
            .ok_or(RecoveryError::Invalid("artifact terminal payload missing"))?;
        if artifact.id != id || reduce::digest(&artifact)? != identity.digest {
            return Err(RecoveryError::Invalid("artifact content binding"));
        }
        Ok(Some(artifact))
    }
    /// Last terminal result's optional artifact id for a retained child owner.
    /// # Errors
    /// Rejects inconsistent source identities.
    pub fn latest_artifact(&self, subagent: &SubagentId) -> Result<Option<String>, RecoveryError> {
        Ok(self
            .binding(subagent)?
            .and_then(|binding| binding.latest_artifact))
    }
    /// Hash complete typed result contents without allocating a serialized copy.
    /// # Errors
    /// Rejects invalid result serialization.
    pub fn result_digest(result: &rw_types::SubagentResult) -> Result<[u8; 32], RecoveryError> {
        reduce::digest(result)
    }
    /// Verify one acknowledged terminal against the latest effective source.
    /// # Errors
    /// Rejects missing, mismatched or stale terminal proof.
    pub fn verify_terminal(
        &self,
        child: &SubagentId,
        session: &SessionId,
        digest: [u8; 32],
    ) -> Result<(), RecoveryError> {
        let binding = self
            .binding(child)?
            .ok_or(RecoveryError::Invalid("acknowledged child has no source"))?;
        if &binding.session_id != session {
            return Err(RecoveryError::Invalid("acknowledged child session"));
        }
        let sequence = binding.terminal.ok_or(RecoveryError::Invalid(
            "acknowledged child lacks terminal proof",
        ))?;
        let event = SourceReader {
            source: &self.source,
            events: VecDeque::new(),
        }
        .event(sequence)?;
        let EngineEvent::SubagentFinished {
            subagent_id,
            result,
            ..
        } = event
        else {
            return Err(RecoveryError::Invalid("child terminal source"));
        };
        if &subagent_id != child
            || &result.subagent_id != child
            || &result.session_id != session
            || reduce::digest(&result)? != digest
        {
            return Err(RecoveryError::Invalid(
                "acknowledged child terminal contents",
            ));
        }
        Ok(())
    }
}
