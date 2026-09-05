use super::{
    CompletedForkOperation, CreateSessionRequest, FORK_JOURNAL_VERSION, ForkJournalResult,
    ForkJournalState, ForkOperationJournal, ForkOperationKey, ForkOperationState,
    ForkSessionRequest, HostError, HostedSession, PreparedForkOperation, RuntimeSessionFactory,
    SESSION_QUERY_DEADLINE, SessionDescriptor, SessionFactory, SessionId, async_trait,
    fork_hosted_session_storage, fs, load_session_metadata_any, new_session_id,
    remove_forked_session_storage, unix_millis,
};

#[async_trait]
impl SessionFactory for RuntimeSessionFactory {
    async fn shutdown(&self) -> Result<(), HostError> {
        self.wasm_workers.shutdown().await;
        self.provider_admission
            .shutdown()
            .await
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        Ok(())
    }

    fn allocate_session_id(&self) -> Result<SessionId, HostError> {
        new_session_id()
            .map(SessionId)
            .map_err(|_| HostError::Persistence("session id allocation failed".to_owned()))
    }

    async fn create(&self, request: CreateSessionRequest) -> Result<HostedSession, HostError> {
        let workspace = self.authorize_workspace(&request.workspace)?;
        self.compose(request.session_id, workspace, request.model, false)
            .await
    }

    async fn resume(&self, session_id: &SessionId) -> Result<HostedSession, HostError> {
        let metadata = load_session_metadata_any(&self.options.storage_root, &session_id.0)
            .map_err(|_| HostError::Persistence("session metadata is unavailable".to_owned()))?;
        let workspace = self.authorize_workspace_path(&metadata.workspace)?;
        self.compose(session_id.clone(), workspace, None, true)
            .await
    }

    async fn load_fork_operation(
        &self,
        key: &ForkOperationKey,
    ) -> Result<ForkOperationState, HostError> {
        let factory = self.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let Some(journal) = factory.load_fork_journal_unlocked(&key)? else {
                return Ok(ForkOperationState::Missing);
            };
            match journal.state {
                ForkJournalState::Prepared | ForkJournalState::StorageCommitted => Ok(
                    ForkOperationState::Pending(Self::journal_operation(&journal)),
                ),
                ForkJournalState::Completed { result } => {
                    Ok(ForkOperationState::Completed(CompletedForkOperation {
                        protocol_version: result.protocol_version,
                        command_ack_emitted_at: result.command_ack_emitted_at,
                        fork_event_emitted_at: result.fork_event_emitted_at,
                        acknowledged_session_id: result.acknowledged_session_id,
                        outcome: result.outcome,
                        parent_session_id: result.parent_session_id,
                        child: result.child,
                        at_turn: result.at_turn,
                    }))
                }
            }
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn prepare_fork_operation(
        &self,
        operation: PreparedForkOperation,
    ) -> Result<PreparedForkOperation, HostError> {
        let workspace = self.workspace_for_session(&operation.request.parent)?;
        let factory = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            if let Some(existing) = factory.load_fork_journal_unlocked(&operation.key)? {
                return Ok(Self::journal_operation(&existing));
            }
            factory.enforce_live_fork_limits_unlocked(false)?;
            if operation.request.operation_key != operation.key {
                return Err(HostError::Protocol(
                    "fork operation key does not match its request".to_owned(),
                ));
            }
            let operation_id = Self::fork_operation_id(&operation.key);
            let child = factory.expected_fork_state(&operation.request, &workspace)?;
            let journal = ForkOperationJournal {
                version: FORK_JOURNAL_VERSION,
                operation_id,
                stable_operation_id: operation.key.operation_id.clone(),
                client_id: operation.key.client_id.clone(),
                request_id: operation.key.request_id.clone(),
                payload_hash: operation.key.payload_hash.clone(),
                updated_unix_ms: unix_millis(),
                parent: operation.request.parent.clone(),
                child_model: child.model,
                child_workspace_generation: child.workspace_generation,
                child_roots_digest: child.roots_digest,
                child_session_id: operation.request.child_session_id.clone(),
                at_turn: operation.request.at_turn.clone(),
                through_sequence: operation.request.through_sequence,
                include_idle_tail: operation.request.include_idle_tail,
                driver_client_id: operation.request.driver_client_id.clone(),
                workspace_digest: blake3::hash(workspace.as_os_str().as_encoded_bytes())
                    .to_hex()
                    .to_string(),
                canonical_workspace: workspace,
                state: ForkJournalState::Prepared,
            };
            let path = factory.fork_journal_path(&operation.key)?;
            match Self::persist_new_fork_journal(&path, &journal) {
                Ok(()) => Ok(operation),
                Err(HostError::RequestConflict) => factory
                    .load_fork_journal_unlocked(&operation.key)?
                    .map(|existing| Self::journal_operation(&existing))
                    .ok_or_else(|| {
                        HostError::Persistence("fork journal creation raced".to_owned())
                    }),
                Err(error) => Err(error),
            }
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn fork(&self, request: ForkSessionRequest) -> Result<HostedSession, HostError> {
        let workspace = self.workspace_for_session(&request.parent)?;
        let through_turn =
            request.at_turn.0.parse::<u64>().map_err(|_| {
                HostError::Protocol("fork turn must be an unsigned decimal".to_owned())
            })?;
        let storage_root = self.options.storage_root.clone();
        let workspace_for_fork = workspace.clone();
        let parent_session_id = request.parent.session_id.0.clone();
        let child_session = request.child_session_id.clone();
        let child_session_id = child_session.0.clone();
        let fork_child_session_id = child_session_id.clone();
        let through_sequence = request.through_sequence;
        let include_idle_tail = request.include_idle_tail;
        let driver_client_id = request.driver_client_id.clone();
        let operation_key = request.operation_key.clone();
        let factory = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let mut journal = factory
                .load_fork_journal_unlocked(&operation_key)?
                .ok_or_else(|| {
                    HostError::Persistence("fork operation was not prepared".to_owned())
                })?;
            if Self::journal_operation(&journal).request != request {
                return Err(HostError::RequestConflict);
            }
            if matches!(journal.state, ForkJournalState::Prepared) {
                // Recompose at commit time so extension changes between
                // prepare and fork cannot bypass the persisted fingerprint.
                let expected = factory.expected_fork_state(&request, &workspace_for_fork)?;
                let operation_id = journal.operation_id.clone();
                fork_hosted_session_storage(
                    &factory.journal_reads,
                    &storage_root,
                    &workspace_for_fork,
                    &parent_session_id,
                    &fork_child_session_id,
                    through_turn,
                    through_sequence,
                    include_idle_tail,
                    driver_client_id,
                    Some(&operation_id),
                    &expected.modes,
                )
                .map_err(|error| {
                    tracing::error!(reason = %error, "session fork storage failed");
                    HostError::Persistence("session fork could not be persisted".to_owned())
                })?;
                journal.state = ForkJournalState::StorageCommitted;
                journal.updated_unix_ms = unix_millis();
                factory.transition_fork_journal_unlocked(&journal)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| HostError::Persistence("fork storage worker failed".to_owned()))??;
        self.compose(child_session, workspace, None, true).await
    }

    async fn complete_fork_operation(
        &self,
        key: &ForkOperationKey,
        result: &CompletedForkOperation,
    ) -> Result<CompletedForkOperation, HostError> {
        let factory = self.clone();
        let key = key.clone();
        let result = result.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let mut journal = factory.load_fork_journal_unlocked(&key)?.ok_or_else(|| {
                HostError::Persistence("fork operation was not prepared".to_owned())
            })?;
            if journal.child_session_id != result.child.session_id
                || journal.parent.session_id != result.parent_session_id
                || journal.at_turn != result.at_turn
            {
                return Err(HostError::SessionIdentityMismatch);
            }
            if let ForkJournalState::Completed { result: existing } = &journal.state {
                if existing.child != result.child || existing.outcome != result.outcome {
                    return Err(HostError::RequestConflict);
                }
                return Ok(Self::completed_fork_result(existing));
            }
            journal.state = ForkJournalState::Completed {
                result: Box::new(ForkJournalResult {
                    protocol_version: result.protocol_version,
                    command_ack_emitted_at: result.command_ack_emitted_at,
                    fork_event_emitted_at: result.fork_event_emitted_at,
                    acknowledged_session_id: result.acknowledged_session_id,
                    outcome: result.outcome,
                    parent_session_id: result.parent_session_id,
                    child: result.child,
                    at_turn: result.at_turn,
                }),
            };
            journal.updated_unix_ms = unix_millis();
            let committed = factory.transition_fork_journal_unlocked(&journal)?;
            factory.enforce_live_fork_limits_unlocked(true)?;
            let ForkJournalState::Completed { result } = committed.state else {
                return Err(HostError::Persistence(
                    "fork completion did not reach its durable phase".to_owned(),
                ));
            };
            Ok(Self::completed_fork_result(&result))
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn abandon_prepared_fork_operation(
        &self,
        key: &ForkOperationKey,
    ) -> Result<(), HostError> {
        let factory = self.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let Some(journal) = factory.load_fork_journal_unlocked(&key)? else {
                return Ok(());
            };
            if !matches!(journal.state, ForkJournalState::Prepared) {
                return Ok(());
            }
            remove_forked_session_storage(
                &factory.options.storage_root,
                &journal.canonical_workspace,
                &journal.child_session_id.0,
            )
            .map_err(|_| HostError::Persistence("partial fork cleanup failed".to_owned()))?;
            let path = factory
                .ensure_fork_journal_directory()?
                .join(format!("{}.json", journal.operation_id));
            fs::remove_file(path).map_err(|_| {
                HostError::Persistence("prepared fork journal cleanup failed".to_owned())
            })?;
            fs::File::open(factory.fork_journal_directory())
                .map_err(|_| {
                    HostError::Persistence("prepared fork directory is unavailable".to_owned())
                })?
                .sync_all()
                .map_err(|_| {
                    HostError::Persistence("prepared fork cleanup could not sync".to_owned())
                })
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn persisted_sessions(&self) -> Result<Vec<SessionDescriptor>, HostError> {
        let factory = self.clone();
        tokio::time::timeout(
            SESSION_QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || factory.persisted_sessions_blocking()),
        )
        .await
        .map_err(|_| HostError::Query("session listing deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("session listing worker failed".to_owned()))?
    }

    async fn search_persisted_sessions(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<SessionDescriptor>, bool), HostError> {
        tokio::time::timeout(
            SESSION_QUERY_DEADLINE,
            self.search_sessions_with_retry(query, limit),
        )
        .await
        .map_err(|_| HostError::Query("session search deadline exceeded".to_owned()))?
    }
}
