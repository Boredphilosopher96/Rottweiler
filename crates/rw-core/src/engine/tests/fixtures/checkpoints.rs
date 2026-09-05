#![cfg(test)]

use crate::engine::AgentLoopError;
use crate::engine::mutation_checkpoints::MutationCheckpoint;
use crate::engine::mutation_checkpoints::MutationCheckpointCoordinator;
use crate::engine::mutation_checkpoints::MutationCheckpointOutcome;
use crate::engine::mutation_checkpoints::RewindCheckpoint;
use async_trait::async_trait;
use rw_tools::MutationScope;
use rw_types::SessionId;
use rw_types::UnrestorablePath;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(Default)]
pub(in crate::engine::tests) struct RecordingCheckpoints {
    pub(in crate::engine::tests) events: Mutex<Vec<String>>,
}

#[async_trait]
impl MutationCheckpointCoordinator for RecordingCheckpoints {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn begin(
        &self,
        session_id: &SessionId,
        _agent_turn: u64,
        tool_call_id: &str,
        scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError> {
        self.events
            .lock()
            .expect("checkpoint lock")
            .push(format!("begin:{}:{tool_call_id}:{scope:?}", session_id.0));
        Ok(MutationCheckpoint {
            id: Some(tool_call_id.to_owned()),
        })
    }

    async fn finish(
        &self,
        checkpoint: &MutationCheckpoint,
        outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError> {
        self.events
            .lock()
            .expect("checkpoint lock")
            .push(format!("finish:{:?}:{outcome:?}", checkpoint.id));
        Ok(())
    }

    async fn prepare_apply_rewind(
        &self,
        session_id: &SessionId,
        to_turn: u64,
        operation_id: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError> {
        self.events
            .lock()
            .expect("checkpoint lock")
            .push(format!("rewind:{}:{to_turn}:{operation_id}", session_id.0));
        Ok(RewindCheckpoint {
            id: operation_id.to_owned(),
            unrestorable_paths: Vec::new(),
        })
    }

    async fn acknowledge_rewind(
        &self,
        checkpoint: &RewindCheckpoint,
    ) -> Result<(), AgentLoopError> {
        self.events
            .lock()
            .expect("checkpoint lock")
            .push(format!("ack:{}", checkpoint.id));
        Ok(())
    }
}

pub(in crate::engine::tests) struct SingleFileCheckpoints {
    pub(in crate::engine::tests) path: PathBuf,
    pub(in crate::engine::tests) snapshots: Mutex<Vec<(u64, Option<Vec<u8>>)>>,
}

#[async_trait]
impl MutationCheckpointCoordinator for SingleFileCheckpoints {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn begin(
        &self,
        _session_id: &SessionId,
        agent_turn: u64,
        tool_call_id: &str,
        _scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError> {
        let before = std::fs::read(&self.path).ok();
        self.snapshots
            .lock()
            .expect("snapshots")
            .push((agent_turn, before));
        Ok(MutationCheckpoint {
            id: Some(tool_call_id.to_owned()),
        })
    }

    async fn finish(
        &self,
        _checkpoint: &MutationCheckpoint,
        _outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn prepare_apply_rewind(
        &self,
        _session_id: &SessionId,
        to_turn: u64,
        operation_id: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError> {
        let snapshot = self
            .snapshots
            .lock()
            .expect("snapshots")
            .iter()
            .filter(|(turn, _)| *turn > to_turn)
            .min_by_key(|(turn, _)| *turn)
            .map(|(_, bytes)| bytes.clone());
        match snapshot {
            Some(Some(bytes)) => std::fs::write(&self.path, bytes)
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?,
            Some(None) => match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(AgentLoopError::Persistence(error.to_string())),
            },
            None => {}
        }
        Ok(RewindCheckpoint {
            id: operation_id.to_owned(),
            unrestorable_paths: Vec::new(),
        })
    }

    async fn acknowledge_rewind(
        &self,
        _checkpoint: &RewindCheckpoint,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

pub(in crate::engine::tests) struct RecordingFileCheckpoints {
    pub(in crate::engine::tests) ordering: Arc<RecordingCheckpoints>,
    pub(in crate::engine::tests) files: SingleFileCheckpoints,
}

#[async_trait]
impl MutationCheckpointCoordinator for RecordingFileCheckpoints {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn begin(
        &self,
        session_id: &SessionId,
        turn: u64,
        call: &str,
        scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError> {
        let checkpoint = self.ordering.begin(session_id, turn, call, scope).await?;
        self.files.begin(session_id, turn, call, scope).await?;
        Ok(checkpoint)
    }

    async fn finish(
        &self,
        checkpoint: &MutationCheckpoint,
        outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError> {
        self.ordering.finish(checkpoint, outcome).await
    }

    async fn prepare_apply_rewind(
        &self,
        session_id: &SessionId,
        turn: u64,
        operation: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError> {
        self.files
            .prepare_apply_rewind(session_id, turn, operation)
            .await
    }

    async fn acknowledge_rewind(
        &self,
        checkpoint: &RewindCheckpoint,
    ) -> Result<(), AgentLoopError> {
        self.files.acknowledge_rewind(checkpoint).await
    }
}

pub(in crate::engine::tests) struct OrderedRewindCoordinator {
    pub(in crate::engine::tests) order: Arc<Mutex<Vec<String>>>,
    pub(in crate::engine::tests) fail_ack: Arc<AtomicBool>,
    pub(in crate::engine::tests) unrestorable_paths: Vec<UnrestorablePath>,
}

#[async_trait]
impl MutationCheckpointCoordinator for OrderedRewindCoordinator {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn begin(
        &self,
        _session_id: &SessionId,
        _agent_turn: u64,
        _tool_call_id: &str,
        _scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError> {
        Ok(MutationCheckpoint { id: None })
    }

    async fn finish(
        &self,
        _checkpoint: &MutationCheckpoint,
        _outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn prepare_apply_rewind(
        &self,
        _session_id: &SessionId,
        _to_turn: u64,
        operation_id: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError> {
        self.order
            .lock()
            .expect("rewind order")
            .push("apply".to_owned());
        Ok(RewindCheckpoint {
            id: operation_id.to_owned(),
            unrestorable_paths: self.unrestorable_paths.clone(),
        })
    }

    async fn acknowledge_rewind(
        &self,
        _checkpoint: &RewindCheckpoint,
    ) -> Result<(), AgentLoopError> {
        self.order
            .lock()
            .expect("rewind order")
            .push("ack".to_owned());
        if self.fail_ack.load(Ordering::SeqCst) {
            Err(AgentLoopError::Persistence(
                "fixture acknowledgement failed".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

pub(in crate::engine::tests) struct InitRecordingCheckpoints {
    pub(in crate::engine::tests) delay: Duration,
    pub(in crate::engine::tests) scopes: Mutex<Vec<MutationScope>>,
    pub(in crate::engine::tests) turns: Mutex<Vec<u64>>,
    pub(in crate::engine::tests) outcomes: Mutex<Vec<MutationCheckpointOutcome>>,
}

impl InitRecordingCheckpoints {
    pub(in crate::engine::tests) fn new(delay: Duration) -> Self {
        Self {
            delay,
            scopes: Mutex::new(Vec::new()),
            turns: Mutex::new(Vec::new()),
            outcomes: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl MutationCheckpointCoordinator for InitRecordingCheckpoints {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn begin(
        &self,
        _session_id: &SessionId,
        agent_turn: u64,
        tool_call_id: &str,
        scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError> {
        self.turns.lock().expect("init turns").push(agent_turn);
        self.scopes.lock().expect("init scopes").push(scope.clone());
        tokio::time::sleep(self.delay).await;
        Ok(MutationCheckpoint {
            id: Some(tool_call_id.to_owned()),
        })
    }

    async fn finish(
        &self,
        _checkpoint: &MutationCheckpoint,
        outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError> {
        self.outcomes.lock().expect("init outcomes").push(outcome);
        Ok(())
    }

    async fn prepare_apply_rewind(
        &self,
        _session_id: &SessionId,
        _to_turn: u64,
        operation_id: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError> {
        Ok(RewindCheckpoint {
            id: operation_id.to_owned(),
            unrestorable_paths: Vec::new(),
        })
    }

    async fn acknowledge_rewind(
        &self,
        _checkpoint: &RewindCheckpoint,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }
}
