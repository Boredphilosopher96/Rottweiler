use futures_util::FutureExt;

use super::{
    Arc, BoundClient, CachedDispatch, ClientCommand, ClientId, ClientRole, CommandMeta,
    CommandOutcome, CompletedForkOperation, DedupeState, EngineHost, ForkOperationKey,
    ForkOperationState, ForkSessionRequest, HostError, PreparedForkOperation, RequestId, SessionId,
    TurnId, command_ack, completed_fork_dispatch, host_error_code, rejected, trim_dedupe, watch,
};

struct ControlRejection(&'static str, &'static str);

enum ControlRequest {
    Complete(Arc<CachedDispatch>),
    Running(watch::Receiver<Option<Arc<CachedDispatch>>>),
    Launch {
        completion: watch::Receiver<Option<Arc<CachedDispatch>>>,
        admission: super::control_admission::ControlLease,
    },
}

impl EngineHost {
    /// Accepted execution and its completion have owners independent of callers
    /// and cache retention. A duplicate never reruns by losing an evicted entry.
    pub(super) async fn dispatch_control(
        &self,
        bound: BoundClient,
        mut command: ClientCommand,
    ) -> CommandOutcome {
        command.meta_mut().client_id = bound.client_id.clone();
        let Ok(plan) = rw_types::allocation::AllocationPlan::new(command) else {
            return rejected("control_limit", "command allocation cannot be bounded");
        };
        let admission = match self.control_admission.acquire(plan.value(), plan.bytes()) {
            Ok(admission) => admission,
            Err(message) => return rejected("control_busy", message),
        };
        let command = plan.prepare().into_inner();
        let Ok(payload_hash) = super::read::command_hash(&command) else {
            return rejected("command_serialization", "command could not serialize");
        };
        let key = (bound.client_id.clone(), command.meta().request_id.clone());
        let request = match self.reserve_control(&key, &payload_hash, admission) {
            Ok(request) => request,
            Err(ControlRejection(code, message)) => return rejected(code, message),
        };
        let (mut completion, owns_execution) = match request {
            ControlRequest::Complete(dispatch) => {
                drop(command);
                self.emit_many(&bound.client_id, &dispatch.events).await;
                return dispatch.outcome.clone();
            }
            ControlRequest::Running(completion) => {
                drop(command);
                (completion, false)
            }
            ControlRequest::Launch {
                completion,
                admission,
            } => {
                self.launch_control(command, key, payload_hash, admission)
                    .await;
                (completion, true)
            }
        };
        loop {
            let completed = completion.borrow_and_update().clone();
            if let Some(dispatch) = completed {
                if !owns_execution {
                    self.emit_many(&bound.client_id, &dispatch.events).await;
                }
                return dispatch.outcome.clone();
            }
            if completion.changed().await.is_err() {
                return rejected(
                    "request_completion_lost",
                    "request completion owner disappeared",
                );
            }
        }
    }

    fn reserve_control(
        &self,
        key: &(ClientId, RequestId),
        payload_hash: &str,
        admission: super::control_admission::ControlLease,
    ) -> Result<ControlRequest, ControlRejection> {
        let mut dedupe = self
            .dedupe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match dedupe.entries.get(key) {
            Some(DedupeState::Read { .. }) => Err(ControlRejection(
                "request_id_conflict",
                "request id was used for a read",
            )),
            Some(DedupeState::Complete {
                payload_hash: existing,
                dispatch,
                retry_same_request,
            }) => {
                if existing != payload_hash {
                    return Err(ControlRejection(
                        "request_id_conflict",
                        "request id was reused with a different command",
                    ));
                }
                let dispatch = Arc::clone(dispatch);
                if *retry_same_request {
                    dedupe.entries.remove(key);
                    dedupe.order.retain(|queued| queued != key);
                }
                Ok(ControlRequest::Complete(dispatch))
            }
            Some(DedupeState::Running {
                payload_hash: existing,
                completion,
            }) => {
                if existing != payload_hash {
                    return Err(ControlRejection(
                        "request_id_conflict",
                        "request id was reused with a different command",
                    ));
                }
                Ok(ControlRequest::Running(completion.subscribe()))
            }
            None => {
                let (completion, wait) = watch::channel(None);
                dedupe.entries.insert(
                    key.clone(),
                    DedupeState::Running {
                        payload_hash: payload_hash.to_owned(),
                        completion,
                    },
                );
                dedupe.order.push_back(key.clone());
                Ok(ControlRequest::Launch {
                    completion: wait,
                    admission,
                })
            }
        }
    }

    async fn launch_control(
        &self,
        command: ClientCommand,
        key: (ClientId, RequestId),
        payload_hash: String,
        admission: super::control_admission::ControlLease,
    ) {
        let shutdown = matches!(command, ClientCommand::ShutdownHost { .. });
        let meta = command.meta().clone();
        let host = self.clone();
        let operation_key = key.clone();
        let operation_hash = payload_hash.clone();
        let work = async move {
            let _admission = admission;
            let dispatch = if let Ok(dispatch) =
                std::panic::AssertUnwindSafe(host.execute(command, operation_hash.clone()))
                    .catch_unwind()
                    .await
            {
                dispatch
            } else {
                host.retain_failed_owners("host control panicked before effect proof".into(), ())
                    .await;
                let outcome = rejected(
                    "control_panicked",
                    "host control failed; effects require host recovery",
                );
                CachedDispatch {
                    events: vec![command_ack(&meta, None, outcome.clone(), &*host.clock)],
                    outcome,
                    cacheable: true,
                }
            };
            host.complete_request(
                operation_key,
                operation_hash,
                Arc::new(dispatch),
                &meta.client_id,
            )
            .await;
        };
        if self.control_owner.spawn(work, shutdown).is_err() {
            let dispatch = Arc::new(CachedDispatch {
                outcome: rejected("host_shutting_down", "host control admission is closed"),
                events: Vec::new(),
                cacheable: true,
            });
            self.complete_request(key.clone(), payload_hash, dispatch, &key.0)
                .await;
        }
    }

    pub(super) async fn complete_request(
        &self,
        key: (ClientId, RequestId),
        payload_hash: String,
        dispatch: Arc<CachedDispatch>,
        client_id: &ClientId,
    ) {
        let completion = {
            let mut dedupe = self
                .dedupe
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let completion = match dedupe.entries.get(&key) {
                Some(DedupeState::Running { completion, .. }) => Some(completion.clone()),
                Some(DedupeState::Complete { .. } | DedupeState::Read { .. }) | None => None,
            };
            dedupe.entries.insert(
                key,
                DedupeState::Complete {
                    payload_hash,
                    dispatch: Arc::clone(&dispatch),
                    retry_same_request: !dispatch.cacheable,
                },
            );
            trim_dedupe(&mut dedupe, self.config.max_deduplicated_requests);
            completion
        };
        self.emit_many(client_id, &dispatch.events).await;
        if let Some(completion) = completion {
            completion.send_replace(Some(dispatch));
        }
    }

    pub(super) async fn execute(
        &self,
        command: ClientCommand,
        payload_hash: String,
    ) -> CachedDispatch {
        let meta = command.meta().clone();
        let command = match command {
            ClientCommand::Fork {
                meta,
                session_id,
                at_turn,
                operation_id,
            } => {
                return self
                    .execute_fork(meta, session_id, at_turn, operation_id, payload_hash)
                    .await;
            }
            command => command,
        };
        let result = self.execute_inner(command).await;
        match result {
            Ok((outcome, session_id, mut events)) => {
                events.insert(
                    0,
                    command_ack(&meta, session_id, outcome.clone(), &*self.clock),
                );
                CachedDispatch {
                    outcome,
                    events,
                    cacheable: true,
                }
            }
            Err(error) => {
                let outcome = rejected(host_error_code(&error), &error.to_string());
                CachedDispatch {
                    events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                    outcome,
                    cacheable: true,
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn execute_fork(
        &self,
        meta: CommandMeta,
        session_id: SessionId,
        at_turn: Option<TurnId>,
        operation_id: String,
        _request_payload_hash: String,
    ) -> CachedDispatch {
        if operation_id.is_empty()
            || operation_id.len() > 128
            || !operation_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            let outcome = rejected(
                "invalid_fork_operation_id",
                "fork operation id must be 1-128 safe ASCII characters",
            );
            return CachedDispatch {
                events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                outcome,
                cacheable: true,
            };
        }
        let Ok(payload) = serde_json::to_vec(&(&session_id, &at_turn)) else {
            let outcome = rejected(
                "fork_payload_serialization",
                "fork operation payload could not serialize",
            );
            return CachedDispatch {
                events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                outcome,
                cacheable: true,
            };
        };
        let payload_hash = blake3::hash(&payload).to_hex().to_string();
        let key = ForkOperationKey {
            operation_id,
            client_id: meta.client_id.clone(),
            request_id: meta.request_id.clone(),
            payload_hash,
        };
        let result = async {
            let mut lifecycle_guard = None;
            let operation = match self.factory.load_fork_operation(&key).await? {
                ForkOperationState::Completed(completed) => return Ok(completed),
                ForkOperationState::Pending(operation) => operation,
                ForkOperationState::Missing => {
                    let parent = self.ready_session(&session_id).await?;
                    lifecycle_guard = Some(Arc::clone(&parent.lifecycle).lock_owned().await);
                    let snapshot = parent.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                        return Err(HostError::Protocol(
                            "only the current driver may fork a session".to_owned(),
                        ));
                    }
                    if snapshot.running
                        || snapshot.active_shell.is_some()
                        || snapshot.active_background
                    {
                        return Err(HostError::Protocol(
                            "forking requires an idle session".to_owned(),
                        ));
                    }
                    let explicit_turn = at_turn.is_some();
                    let resolved_turn = if let Some(turn) = &at_turn {
                        let turn = turn.0.parse::<u64>().map_err(|_| {
                            HostError::Protocol("fork turn must be an unsigned decimal".to_owned())
                        })?;
                        if turn == 0 || turn > snapshot.completed_turns {
                            return Err(HostError::Protocol(
                                "fork turn is not a completed parent boundary".to_owned(),
                            ));
                        }
                        turn
                    } else {
                        snapshot.completed_turns
                    };
                    let through_sequence = if explicit_turn {
                        None
                    } else {
                        let tail = parent.handle().last_sequence().await?;
                        let verified = parent.handle().snapshot().await?;
                        let verified_tail = parent.handle().last_sequence().await?;
                        if verified.running
                            || verified.active_shell.is_some()
                            || verified.active_background
                            || verified.completed_turns != snapshot.completed_turns
                            || verified.driver_client_id != snapshot.driver_client_id
                            || verified_tail != tail
                        {
                            return Err(HostError::Protocol(
                                "parent changed while the fork boundary was captured; retry"
                                    .to_owned(),
                            ));
                        }
                        tail
                    };
                    self.factory
                        .prepare_fork_operation(PreparedForkOperation {
                            key: key.clone(),
                            request: ForkSessionRequest {
                                operation_key: key.clone(),
                                parent: parent.descriptor(),
                                child_session_id: self.factory.allocate_session_id()?,
                                at_turn: TurnId(resolved_turn.to_string()),
                                through_sequence,
                                include_idle_tail: !explicit_turn,
                                driver_client_id: meta.client_id.clone(),
                            },
                        })
                        .await?
                }
            };
            let child_session_id = operation.request.child_session_id.clone();
            let child = match self.fork_session(operation.request.clone()).await {
                Ok(child) => child,
                Err(error @ (HostError::SessionCapacity | HostError::ShuttingDown)) => {
                    self.factory.abandon_prepared_fork_operation(&key).await?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            let attach = if operation.request.driver_client_id == meta.client_id {
                ClientCommand::AttachSession {
                    meta: meta.clone(),
                    session_id: child_session_id,
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                }
            } else {
                ClientCommand::TakeDriver {
                    meta: meta.clone(),
                    session_id: child_session_id,
                }
            };
            let outcome = child.handle().dispatch(attach).await?;
            if !matches!(outcome, CommandOutcome::Accepted {}) {
                return Err(HostError::Persistence(
                    "fork child could not attach its authorized driver".to_owned(),
                ));
            }
            child.set_driver(Some(meta.client_id.clone()));
            let completed = CompletedForkOperation {
                protocol_version: rw_types::PROTOCOL_VERSION,
                command_ack_emitted_at: self.clock.emitted_at(),
                fork_event_emitted_at: self.clock.emitted_at(),
                acknowledged_session_id: session_id.clone(),
                outcome,
                parent_session_id: session_id,
                child: child.descriptor(),
                at_turn: operation.request.at_turn,
            };
            let completed = self
                .factory
                .complete_fork_operation(&key, &completed)
                .await?;
            drop(lifecycle_guard);
            Ok(completed)
        }
        .await;
        match result {
            Ok(completed) => completed_fork_dispatch(&key, completed),
            Err(error) => {
                let outcome = rejected(host_error_code(&error), &error.to_string());
                CachedDispatch {
                    events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                    outcome,
                    // A durable operation may already exist. Never strand it behind
                    // a process-local cached failure; the same request may retry.
                    cacheable: false,
                }
            }
        }
    }
}
