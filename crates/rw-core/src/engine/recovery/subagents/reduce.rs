use super::{
    ARTIFACT_IDENTITIES, ARTIFACTS, ArtifactIdentity, BOUNDARIES, Head, IDENTITIES, PENDING,
    RAW_SPAWNS, RecoveryError, STATES, SubagentBinding, TURN_EVENTS, TURN_KEYS, VERSIONS, identity,
    key, raw_identity,
};
use crate::engine::recovery::projector::BatchRows;
use rw_types::{EngineEvent, SequenceId};

pub(super) fn apply(
    head: &mut Head,
    rows: &mut BatchRows,
    sequence: SequenceId,
    event: &EngineEvent,
) -> Result<(), RecoveryError> {
    let meta = event
        .meta()
        .ok_or(RecoveryError::Invalid("non-durable child source"))?;
    if meta.protocol_version != crate::engine::SESSION_EVENT_VERSION
        || meta.sequence_id != sequence
        || sequence.0 != head.next
        || head
            .session
            .as_ref()
            .is_some_and(|session| session != &meta.session_id)
    {
        return Err(RecoveryError::Invalid("child source envelope"));
    }
    head.session = Some(meta.session_id.clone());
    match event {
        EngineEvent::TurnStarted { turn_id, .. } => head.active_turn = Some(turn(turn_id)?),
        EngineEvent::TurnFinished { turn_id, .. } => {
            if head.active_turn != Some(turn(turn_id)?) {
                return Err(RecoveryError::Invalid("child source turn lifecycle"));
            }
            rows.put(key(BOUNDARIES, 0, turn(turn_id)?), &sequence)?;
            head.active_turn = None;
        }
        EngineEvent::ConversationRewound { to_agent_turn, .. } => {
            if rows
                .get::<SequenceId>(key(BOUNDARIES, 0, *to_agent_turn))?
                .is_none()
            {
                return Err(RecoveryError::Invalid("child rewind boundary"));
            }
            head.active_turn = None;
            head.rewind = Some((sequence, *to_agent_turn));
            return Ok(());
        }
        EngineEvent::SubagentSpawned {
            subagent_id,
            child_session_id,
            task,
            ..
        } => {
            spawn(head, rows, sequence, subagent_id, child_session_id, task)?;
        }
        EngineEvent::SubagentFinished {
            subagent_id,
            result,
            ..
        } => {
            finish(head, rows, sequence, subagent_id, result)?;
        }
        _ => {}
    }
    head.next = sequence
        .0
        .checked_add(1)
        .ok_or(RecoveryError::Invalid("child sequence overflow"))?;
    Ok(())
}
fn spawn(
    head: &Head,
    rows: &mut BatchRows,
    sequence: SequenceId,
    subagent_id: &rw_types::SubagentId,
    child_session_id: &rw_types::SessionId,
    task: &str,
) -> Result<(), RecoveryError> {
    let turn = head
        .active_turn
        .ok_or(RecoveryError::Invalid("child spawn outside active turn"))?;
    let id = identity(subagent_id)?;
    let scope = if let Some(scope) = rows.lookup(IDENTITIES, id)? {
        scope
    } else {
        rows.put_lookup(IDENTITIES, id.to_vec(), &sequence.0)?;
        sequence.0
    };
    let current: Option<SubagentBinding> = rows.get(key(STATES, 0, scope))?;
    if current
        .as_ref()
        .is_some_and(|current| current.terminal.is_none())
    {
        return Err(RecoveryError::Invalid("child spawned without terminal"));
    }
    rows.put_lookup(
        RAW_SPAWNS,
        raw_identity(subagent_id, child_session_id)?,
        &sequence,
    )?;
    let next = SubagentBinding {
        subagent_id: subagent_id.clone(),
        session_id: child_session_id.clone(),
        spawned: sequence,
        spawned_turn: turn,
        task_preview: task[..task.floor_char_boundary(
            rw_types::session_children::MAX_CHILD_TASK_PREVIEW_BYTES.min(task.len()),
        )]
            .to_owned(),
        task_truncated: task.len() > rw_types::session_children::MAX_CHILD_TASK_PREVIEW_BYTES,
        terminal: None,
        latest_artifact: current
            .as_ref()
            .filter(|current| &current.session_id == child_session_id)
            .and_then(|current| current.latest_artifact.clone()),
        latest_result: current
            .filter(|current| &current.session_id == child_session_id)
            .and_then(|current| current.latest_result),
        scope,
        revision: sequence,
        artifact_scope: None,
    };
    version(rows, turn, &next)?;
    publish(rows, &next)?;
    Ok(())
}
fn finish(
    head: &Head,
    rows: &mut BatchRows,
    sequence: SequenceId,
    subagent_id: &rw_types::SubagentId,
    result: &rw_types::SubagentResult,
) -> Result<(), RecoveryError> {
    let scope = rows
        .lookup(IDENTITIES, identity(subagent_id)?)?
        .ok_or(RecoveryError::Invalid("child result without spawn"))?;
    let mut current: SubagentBinding =
        rows.get(key(STATES, 0, scope))?
            .ok_or(RecoveryError::Invalid(
                "child result without effective spawn",
            ))?;
    if current.terminal.is_some()
        || current.session_id != result.session_id
        || &result.subagent_id != subagent_id
    {
        return Err(RecoveryError::Invalid("child terminal identity"));
    }
    current.terminal = Some(sequence);
    current.latest_result = Some(sequence);
    current.latest_artifact = result
        .diff_artifact
        .as_ref()
        .map(|artifact| artifact.id.clone());
    current.revision = sequence;
    if let Some(artifact) = &result.diff_artifact {
        let id = artifact.id.as_bytes();
        if id.is_empty() || id.len() > 256 {
            return Err(RecoveryError::Invalid("child artifact identity"));
        }
        let digest = digest(artifact)?;
        let bound = if let Some(bound) = rows.lookup::<ArtifactIdentity>(ARTIFACT_IDENTITIES, id)? {
            if bound.digest != digest {
                return Err(RecoveryError::Invalid("artifact identity changed contents"));
            }
            bound
        } else {
            let bound = ArtifactIdentity {
                scope: sequence.0,
                digest,
            };
            rows.put_lookup(ARTIFACT_IDENTITIES, id.to_vec(), &bound)?;
            bound
        };
        current.artifact_scope = Some(bound.scope);
        rows.put(key(ARTIFACTS, bound.scope, sequence.0), &current.scope)?;
    }
    let turn = head.active_turn.unwrap_or(current.spawned_turn);
    rows.delete(key(PENDING, 0, current.spawned.0));
    version(rows, turn, &current)?;
    publish(rows, &current)?;
    Ok(())
}

fn version(rows: &mut BatchRows, turn: u64, state: &SubagentBinding) -> Result<(), RecoveryError> {
    rows.put(key(VERSIONS, state.scope, state.revision.0), state)?;
    rows.put(key(TURN_KEYS, 0, turn), &())?;
    rows.put(key(TURN_EVENTS, turn, state.revision.0), &state.scope)
}
pub(super) fn publish(rows: &mut BatchRows, state: &SubagentBinding) -> Result<(), RecoveryError> {
    rows.put(key(STATES, 0, state.scope), state)?;
    if state.terminal.is_none() {
        rows.put(key(PENDING, 0, state.spawned.0), &state.scope)?;
    }
    Ok(())
}
fn turn(turn: &rw_types::TurnId) -> Result<u64, RecoveryError> {
    turn.0
        .parse()
        .map_err(|_| RecoveryError::Invalid("child source turn identity"))
}
pub(super) fn digest(value: &impl serde::Serialize) -> Result<[u8; 32], RecoveryError> {
    struct HashWriter(blake3::Hasher);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(blake3::Hasher::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(*writer.0.finalize().as_bytes())
}
