use super::{
    REWIND_COORDINATOR_VERSION, RewindCoordinatorDecision, RewindCoordinatorState,
    load_rewind_coordinator, persist_rewind_coordinator, remove_rewind_coordinator,
    validate_rewind_coordinator,
};
use async_trait::async_trait;
use paths::{
    checkpoint_display_path, group_checkpoint_paths, merge_root_reviews,
    resolve_review_display_path,
};
use rw_core::{
    AgentLoopError, MutationCheckpoint, MutationCheckpointCoordinator, MutationCheckpointOutcome,
    ReviewFileDecision, RewindCheckpoint, SessionReview, UnrestorablePath,
};
use rw_store::checkpoint::{CheckpointStore, OpaqueMutation, RewindHandle};
use rw_tools::MutationScope;
use rw_types::SessionId;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

mod paths;
mod recovery;
pub(super) use recovery::recover_rewind_transactions;

enum ActiveCheckpointState {
    Known,
    Opaque(Vec<(usize, OpaqueMutation)>),
}

struct ActiveCheckpoint {
    state: ActiveCheckpointState,
    _workspace_guard: tokio::sync::OwnedMutexGuard<()>,
}

struct ActiveRewind {
    handles: Vec<RewindHandle>,
    target_turn: u64,
    _workspace_guard: tokio::sync::OwnedMutexGuard<()>,
}

struct WorkspaceMutationState {
    lock: Arc<tokio::sync::Mutex<()>>,
    poisoned: Arc<AtomicBool>,
}

impl WorkspaceMutationState {
    fn new() -> Self {
        Self {
            lock: Arc::new(tokio::sync::Mutex::new(())),
            poisoned: Arc::new(AtomicBool::new(false)),
        }
    }
}

fn shared_workspace_mutation_state(workspace: &Path) -> Arc<WorkspaceMutationState> {
    static STATES: OnceLock<Mutex<HashMap<PathBuf, std::sync::Weak<WorkspaceMutationState>>>> =
        OnceLock::new();
    let mut states = STATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = states.get(workspace).and_then(std::sync::Weak::upgrade) {
        return state;
    }
    let state = Arc::new(WorkspaceMutationState::new());
    states.insert(workspace.to_path_buf(), Arc::downgrade(&state));
    state
}

pub(super) struct DurableCheckpointCoordinator {
    checkpoint_root: PathBuf,
    stores: Arc<Vec<Arc<CheckpointStore>>>,
    workspace_mutation: Arc<WorkspaceMutationState>,
    active: Mutex<HashMap<String, ActiveCheckpoint>>,
    rewinds: Mutex<HashMap<String, ActiveRewind>>,
    #[cfg(test)]
    fail_after_committed_rewind_decision: AtomicBool,
    #[cfg(test)]
    fail_rewind_apply_root: AtomicUsize,
    #[cfg(test)]
    fail_rewind_apply_persistently: AtomicBool,
}

impl DurableCheckpointCoordinator {
    #[cfg(test)]
    pub(super) fn new(checkpoint_root: PathBuf, store: Arc<CheckpointStore>) -> Self {
        Self::from_stores(checkpoint_root, Arc::new(vec![store]))
    }

    pub(super) fn from_stores(
        checkpoint_root: PathBuf,
        stores: Arc<Vec<Arc<CheckpointStore>>>,
    ) -> Self {
        let workspace_mutation = stores.first().map_or_else(
            || Arc::new(WorkspaceMutationState::new()),
            |store| shared_workspace_mutation_state(store.workspace_root()),
        );
        Self {
            checkpoint_root,
            stores,
            workspace_mutation,
            active: Mutex::new(HashMap::new()),
            rewinds: Mutex::new(HashMap::new()),
            #[cfg(test)]
            fail_after_committed_rewind_decision: AtomicBool::new(false),
            #[cfg(test)]
            fail_rewind_apply_root: AtomicUsize::new(usize::MAX),
            #[cfg(test)]
            fail_rewind_apply_persistently: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(super) fn fail_after_committed_rewind_decision(&self) {
        self.fail_after_committed_rewind_decision
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(super) fn fail_rewind_apply_at_root(&self, root_index: usize, persistently: bool) {
        self.fail_rewind_apply_root
            .store(root_index, Ordering::SeqCst);
        self.fail_rewind_apply_persistently
            .store(persistently, Ordering::SeqCst);
    }

    fn ensure_workspace_consistent(&self) -> std::result::Result<(), AgentLoopError> {
        if self.workspace_mutation.poisoned.load(Ordering::Acquire) {
            return Err(AgentLoopError::Persistence(
                "workspace mutations are blocked until committed rewind recovery completes"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct RewindApplyFault {
    root_index: Option<usize>,
    persistent: bool,
}

fn prepare_coordinated_rewind(
    checkpoint_root: &Path,
    stores: &[Arc<CheckpointStore>],
    session_id: &str,
    operation_id: &str,
    target_turn: u64,
) -> std::result::Result<Vec<RewindHandle>, AgentLoopError> {
    if load_rewind_coordinator(checkpoint_root)
        .map_err(|error| {
            AgentLoopError::Persistence(format!(
                "rewind coordinator could not be inspected: {error}"
            ))
        })?
        .is_some()
    {
        return Err(AgentLoopError::Persistence(
            "another rewind coordinator decision is pending".to_owned(),
        ));
    }
    let mut decision = RewindCoordinatorDecision {
        version: REWIND_COORDINATOR_VERSION,
        session_id: session_id.to_owned(),
        operation_id: operation_id.to_owned(),
        target_turn,
        root_count: stores.len(),
        state: RewindCoordinatorState::Preparing,
    };
    validate_rewind_coordinator(&decision).map_err(|error| {
        AgentLoopError::Persistence(format!("rewind coordinator is invalid: {error}"))
    })?;
    persist_rewind_coordinator(checkpoint_root, &decision).map_err(|error| {
        AgentLoopError::Persistence(format!(
            "rewind preparation decision could not persist: {error}"
        ))
    })?;
    let mut handles = Vec::with_capacity(stores.len());
    for store in stores {
        match store.prepare_rewind(session_id, target_turn, operation_id) {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                let preparation_error = checkpoint_agent_error(error);
                for (prepared_store, handle) in stores.iter().zip(&handles) {
                    prepared_store
                        .discard_prepared_rewind(handle, target_turn)
                        .map_err(checkpoint_agent_error)?;
                }
                remove_rewind_coordinator(checkpoint_root).map_err(|cleanup_error| {
                    AgentLoopError::Persistence(format!(
                        "{preparation_error}; rewind preparation cleanup failed: {cleanup_error}"
                    ))
                })?;
                return Err(preparation_error);
            }
        }
    }
    decision.state = RewindCoordinatorState::Committed;
    persist_rewind_coordinator(checkpoint_root, &decision).map_err(|error| {
        AgentLoopError::Persistence(format!("rewind commit decision could not persist: {error}"))
    })?;
    Ok(handles)
}

fn apply_coordinated_rewind(
    stores: &[Arc<CheckpointStore>],
    handles: &[RewindHandle],
    fault: RewindApplyFault,
) -> std::result::Result<Vec<UnrestorablePath>, AgentLoopError> {
    let mut failure_injected = false;
    let mut apply_all = || {
        let mut unrestorable_paths = Vec::new();
        for (root_index, (store, handle)) in stores.iter().zip(handles).enumerate() {
            if fault.root_index == Some(root_index) && (fault.persistent || !failure_injected) {
                failure_injected = true;
                return Err(AgentLoopError::Persistence(format!(
                    "injected rewind apply failure at root {root_index}"
                )));
            }
            let commit = store.apply_rewind(handle).map_err(checkpoint_agent_error)?;
            unrestorable_paths.extend(commit.report.unrestorable.into_iter().map(
                |(path, reason)| UnrestorablePath {
                    path: checkpoint_display_path(root_index, &path),
                    reason,
                },
            ));
        }
        Ok(unrestorable_paths)
    };
    match apply_all() {
        Ok(paths) => Ok(paths),
        Err(first_error) => apply_all().map_err(|recovery_error| {
            AgentLoopError::Persistence(format!(
                "{first_error}; immediate committed rewind recovery failed: {recovery_error}"
            ))
        }),
    }
}

#[async_trait]
impl MutationCheckpointCoordinator for DurableCheckpointCoordinator {
    async fn begin(
        &self,
        session_id: &SessionId,
        agent_turn: u64,
        tool_call_id: &str,
        scope: &MutationScope,
    ) -> std::result::Result<MutationCheckpoint, AgentLoopError> {
        if matches!(scope, MutationScope::None) {
            return Ok(MutationCheckpoint { id: None });
        }
        self.ensure_workspace_consistent()?;
        let workspace_guard = Arc::clone(&self.workspace_mutation.lock).lock_owned().await;
        self.ensure_workspace_consistent()?;
        let session_id = session_id.0.clone();
        let scope = scope.clone();
        let stores = Arc::clone(&self.stores);
        let active = tokio::task::spawn_blocking(move || {
            Ok::<_, AgentLoopError>(match scope {
                MutationScope::None => unreachable!("none returned before the worker"),
                MutationScope::Paths(paths) => {
                    let grouped = group_checkpoint_paths(&stores, paths)?;
                    for (root_index, paths) in grouped {
                        stores[root_index]
                            .checkpoint_known(&session_id, agent_turn, paths)
                            .map_err(checkpoint_agent_error)?;
                    }
                    ActiveCheckpointState::Known
                }
                MutationScope::OpaqueWorkspace => {
                    let mut mutations = Vec::with_capacity(stores.len());
                    for (root_index, store) in stores.iter().enumerate() {
                        mutations.push((
                            root_index,
                            store
                                .begin_opaque_mutation(&session_id, agent_turn)
                                .map_err(checkpoint_agent_error)?,
                        ));
                    }
                    ActiveCheckpointState::Opaque(mutations)
                }
            })
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))??;
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                tool_call_id.to_owned(),
                ActiveCheckpoint {
                    state: active,
                    _workspace_guard: workspace_guard,
                },
            );
        Ok(MutationCheckpoint {
            id: Some(tool_call_id.to_owned()),
        })
    }

    async fn finish(
        &self,
        checkpoint: &MutationCheckpoint,
        _outcome: MutationCheckpointOutcome,
    ) -> std::result::Result<(), AgentLoopError> {
        let Some(id) = &checkpoint.id else {
            return Ok(());
        };
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .ok_or_else(|| AgentLoopError::Persistence("unknown mutation checkpoint".to_owned()))?;
        if let ActiveCheckpointState::Opaque(mutations) = active.state {
            let stores = Arc::clone(&self.stores);
            tokio::task::spawn_blocking(move || {
                for (root_index, mutation) in mutations {
                    stores[root_index].finish_opaque_mutation(&mutation)?;
                }
                Ok::<_, rw_store::checkpoint::CheckpointError>(())
            })
            .await
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
            .map_err(checkpoint_agent_error)?;
        }
        Ok(())
    }

    async fn prepare_apply_rewind(
        &self,
        session_id: &SessionId,
        to_turn: u64,
        operation_id: &str,
    ) -> std::result::Result<RewindCheckpoint, AgentLoopError> {
        self.ensure_workspace_consistent()?;
        let workspace_guard = Arc::clone(&self.workspace_mutation.lock).lock_owned().await;
        self.ensure_workspace_consistent()?;
        let stores = Arc::clone(&self.stores);
        let checkpoint_root = self.checkpoint_root.clone();
        let workspace_poisoned = Arc::clone(&self.workspace_mutation.poisoned);
        let session_id = session_id.0.clone();
        let operation_id_owned = operation_id.to_owned();
        #[cfg(test)]
        let fail_after_committed_decision = self
            .fail_after_committed_rewind_decision
            .swap(false, Ordering::SeqCst);
        #[cfg(not(test))]
        let fail_after_committed_decision = false;
        #[cfg(test)]
        let fail_rewind_apply_root = match self
            .fail_rewind_apply_root
            .swap(usize::MAX, Ordering::SeqCst)
        {
            usize::MAX => None,
            root_index => Some(root_index),
        };
        #[cfg(test)]
        let fail_rewind_apply_persistently = self
            .fail_rewind_apply_persistently
            .swap(false, Ordering::SeqCst);
        #[cfg(not(test))]
        let fail_rewind_apply_root = None;
        #[cfg(not(test))]
        let fail_rewind_apply_persistently = false;
        let (handles, unrestorable_paths) = tokio::task::spawn_blocking(move || {
            let handles = prepare_coordinated_rewind(
                &checkpoint_root,
                &stores,
                &session_id,
                &operation_id_owned,
                to_turn,
            )?;
            if fail_after_committed_decision {
                return Err(AgentLoopError::Persistence(
                    "injected crash after committed rewind decision".to_owned(),
                ));
            }
            let unrestorable_paths = match apply_coordinated_rewind(
                &stores,
                &handles,
                RewindApplyFault {
                    root_index: fail_rewind_apply_root,
                    persistent: fail_rewind_apply_persistently,
                },
            ) {
                Ok(paths) => paths,
                Err(error) => {
                    workspace_poisoned.store(true, Ordering::Release);
                    return Err(error);
                }
            };
            Ok::<_, AgentLoopError>((handles, unrestorable_paths))
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))??;
        self.rewinds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                operation_id.to_owned(),
                ActiveRewind {
                    handles,
                    target_turn: to_turn,
                    _workspace_guard: workspace_guard,
                },
            );
        Ok(RewindCheckpoint {
            id: operation_id.to_owned(),
            unrestorable_paths,
        })
    }

    async fn acknowledge_rewind(
        &self,
        checkpoint: &RewindCheckpoint,
    ) -> std::result::Result<(), AgentLoopError> {
        let rewind = self
            .rewinds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&checkpoint.id)
            .ok_or_else(|| AgentLoopError::Persistence("unknown rewind checkpoint".to_owned()))?;
        let handles = rewind.handles;
        let target_turn = rewind.target_turn;
        let stores = Arc::clone(&self.stores);
        let checkpoint_root = self.checkpoint_root.clone();
        let operation_id = checkpoint.id.clone();
        tokio::task::spawn_blocking(move || {
            if handles.len() != stores.len() {
                return Err(AgentLoopError::Persistence(
                    "rewind root count differs from coordinator".to_owned(),
                ));
            }
            let decision = load_rewind_coordinator(&checkpoint_root)
                .map_err(checkpoint_agent_error)?
                .ok_or_else(|| {
                    AgentLoopError::Persistence(
                        "committed rewind coordinator is missing".to_owned(),
                    )
                })?;
            if decision.state != RewindCoordinatorState::Committed
                || decision.operation_id != operation_id
                || decision.target_turn != target_turn
                || decision.root_count != stores.len()
                || handles
                    .iter()
                    .any(|handle| handle.session_id != decision.session_id)
            {
                return Err(AgentLoopError::Persistence(
                    "committed rewind coordinator identity differs".to_owned(),
                ));
            }
            for (store, handle) in stores.iter().zip(&handles) {
                store
                    .acknowledge_rewind(handle)
                    .map_err(checkpoint_agent_error)?;
            }
            remove_rewind_coordinator(&checkpoint_root).map_err(|error| {
                AgentLoopError::Persistence(format!(
                    "rewind coordinator acknowledgement failed: {error}"
                ))
            })
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
    }

    async fn session_review(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<SessionReview, AgentLoopError> {
        self.ensure_workspace_consistent()?;
        let _workspace_guard = Arc::clone(&self.workspace_mutation.lock).lock_owned().await;
        self.ensure_workspace_consistent()?;
        let stores = Arc::clone(&self.stores);
        let session_id = session_id.clone();
        tokio::task::spawn_blocking(move || {
            let reviews = stores
                .iter()
                .map(|store| {
                    store
                        .session_review(&session_id.0)
                        .map_err(checkpoint_agent_error)
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            merge_root_reviews(session_id, reviews)
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
    }

    async fn resolve_review_file(
        &self,
        session_id: &SessionId,
        path: &Path,
        decision: ReviewFileDecision,
        current_hash: &str,
    ) -> std::result::Result<SessionReview, AgentLoopError> {
        self.ensure_workspace_consistent()?;
        let _workspace_guard = Arc::clone(&self.workspace_mutation.lock).lock_owned().await;
        self.ensure_workspace_consistent()?;
        let stores = Arc::clone(&self.stores);
        let session_id = session_id.clone();
        let path = path.to_path_buf();
        let current_hash = current_hash.to_owned();
        tokio::task::spawn_blocking(move || {
            let (root_index, relative) = resolve_review_display_path(stores.len(), &path)?;
            let mut reviews = stores
                .iter()
                .map(|store| {
                    store
                        .session_review(&session_id.0)
                        .map_err(checkpoint_agent_error)
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let _ = merge_root_reviews(session_id.clone(), reviews.clone())?;
            let target_review = stores[root_index]
                .resolve_review_file(&session_id.0, &relative, decision, &current_hash)
                .map_err(checkpoint_agent_error)?;
            reviews[root_index] = target_review;
            merge_root_reviews(session_id, reviews)
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
    }
}

fn checkpoint_agent_error(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(format!("checkpoint store failed: {error}"))
}
