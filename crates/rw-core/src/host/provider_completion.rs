use super::{
    Arc, CommandMeta, EngineEvent, EngineHost, HostError, PROVIDER_AUTH_COMPLETE_DEADLINE,
    ProviderAuthAttempt, ProviderAuthAttemptId, ProviderAuthCompletionGuard, ProviderAuthOwner,
    ack_meta, overlay_model_catalog_current, provider_catalog_is_ready,
    sanitized_persisted_provider_auth_warnings, sanitized_provider_auth_error,
    transition_provider_auth_to_finalizing, validate_provider_auth_completion, watch,
};

impl EngineHost {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn complete_provider_auth_task(
        self,
        owner: ProviderAuthOwner,
        attempt_id: ProviderAuthAttemptId,
        attempt: ProviderAuthAttempt,
        mut cancel_signal: watch::Receiver<bool>,
        meta: CommandMeta,
    ) {
        let _reservation_guard = ProviderAuthCompletionGuard {
            pending: Arc::clone(&self.provider_auth),
            owner: owner.clone(),
            attempt_id: attempt_id.clone(),
        };
        let cancellation = attempt.cancellation();
        let completion = tokio::select! {
            result = tokio::time::timeout(PROVIDER_AUTH_COMPLETE_DEADLINE, attempt.complete()) => {
                result.unwrap_or_else(|_| {
                    cancellation();
                    Err(HostError::Query("provider authentication deadline exceeded".to_owned()))
                })
            }
            changed = cancel_signal.changed() => {
                let _ = changed;
                Err(HostError::Query("provider authentication was cancelled".to_owned()))
            }
        };
        let mut completion = match completion
            .and_then(|completion| validate_provider_auth_completion(&owner.provider, completion))
        {
            Ok(completion) => completion,
            Err(error) => {
                self.emit_provider_auth_finished(
                    &owner,
                    attempt_id,
                    &meta,
                    false,
                    sanitized_provider_auth_error(&error),
                    Vec::new(),
                )
                .await;
                return;
            }
        };
        let provider_mutation = Arc::clone(&self.provider_mutation).lock_owned().await;
        let session = match self.ready_session(&owner.session_id).await {
            Ok(session) => session,
            Err(error) => {
                drop(provider_mutation);
                self.emit_provider_auth_finished(
                    &owner,
                    attempt_id,
                    &meta,
                    false,
                    sanitized_provider_auth_error(&error),
                    Vec::new(),
                )
                .await;
                return;
            }
        };
        let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
        let snapshot = match session.handle().snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                drop(lifecycle_guard);
                drop(provider_mutation);
                self.emit_provider_auth_finished(
                    &owner,
                    attempt_id,
                    &meta,
                    false,
                    sanitized_provider_auth_error(&HostError::from(error)),
                    Vec::new(),
                )
                .await;
                return;
            }
        };
        if snapshot.driver_client_id.as_ref() != Some(&owner.client_id)
            || !transition_provider_auth_to_finalizing(&self.provider_auth, &owner, &attempt_id)
        {
            drop(lifecycle_guard);
            drop(provider_mutation);
            return;
        }

        // This transition is the irreversible boundary. The host-owned task
        // now holds both the global mutation lock and the session lifecycle;
        // disconnect and takeover may no longer cancel or interleave the write.
        let persisted = if let Some(persistence) = completion.take_persistence() {
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, persistence)
                .await
                .map_err(|_| {
                    HostError::Persistence("provider credential storage failed".to_owned())
                })
                .and_then(std::convert::identity)
        } else {
            Ok(Vec::new())
        };
        let (message, warnings) = match persisted {
            Ok(mut persisted_warnings) => {
                completion.warnings.append(&mut persisted_warnings);
                (
                    completion.message,
                    sanitized_persisted_provider_auth_warnings(completion.warnings),
                )
            }
            Err(error) => {
                drop(lifecycle_guard);
                drop(provider_mutation);
                self.emit_provider_auth_finished(
                    &owner,
                    attempt_id,
                    &meta,
                    false,
                    sanitized_provider_auth_error(&error),
                    Vec::new(),
                )
                .await;
                return;
            }
        };
        // Credential persistence is the authentication result. Report it
        // immediately; activation and catalog discovery are independent
        // readiness work and must not relabel or delay a successful login.
        self.emit_provider_auth_finished(&owner, attempt_id, &meta, true, message, warnings)
            .await;
        let activated = session
            .handle()
            .activate_provider(&owner.provider, Some(&snapshot.model_alias))
            .await
            .is_ok();
        drop(lifecycle_guard);
        drop(provider_mutation);
        let catalog_ready = self
            .emit_refreshed_provider_catalog(&owner, &meta, snapshot)
            .await;
        let (ready, readiness_message) = match (activated, catalog_ready) {
            (true, Some(true)) => (
                true,
                "Provider connected. Choose a model from /models.".to_owned(),
            ),
            (false, _) => (
                false,
                "Signed in, but the provider connection is not ready. Retry from /providers."
                    .to_owned(),
            ),
            (true, None) => (
                false,
                "Signed in, but the model catalog could not be refreshed. Retry from /providers."
                    .to_owned(),
            ),
            (true, Some(false)) => (
                false,
                "Signed in, but this provider is not reachable or returned no models. Retry from /providers."
                    .to_owned(),
            ),
        };
        self.emit_provider_activation_finished(&owner, &meta, ready, readiness_message)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn emit_provider_auth_finished(
        &self,
        owner: &ProviderAuthOwner,
        attempt_id: ProviderAuthAttemptId,
        meta: &CommandMeta,
        success: bool,
        message: String,
        warnings: Vec<String>,
    ) {
        self.emit_many(
            &owner.client_id,
            &[EngineEvent::ProviderAuthFinished {
                meta: ack_meta(meta, &*self.clock),
                session_id: owner.session_id.clone(),
                attempt_id,
                provider: owner.provider.clone(),
                success,
                message,
                warnings,
            }],
        )
        .await;
    }

    pub(super) async fn emit_provider_activation_finished(
        &self,
        owner: &ProviderAuthOwner,
        meta: &CommandMeta,
        success: bool,
        message: String,
    ) {
        self.emit_many(
            &owner.client_id,
            &[EngineEvent::ProviderActivationFinished {
                meta: ack_meta(meta, &*self.clock),
                session_id: owner.session_id.clone(),
                provider: owner.provider.clone(),
                success,
                message,
            }],
        )
        .await;
    }

    pub(super) async fn emit_refreshed_provider_catalog(
        &self,
        owner: &ProviderAuthOwner,
        meta: &CommandMeta,
        snapshot: crate::SessionSnapshot,
    ) -> Option<bool> {
        let selected = Some(snapshot.model_alias.as_str());
        let resolved = snapshot.resolved_model.as_deref();
        // Authentication is a provider-scoped action. Refreshing the global
        // catalog here would resolve unrelated credentials (and can trigger
        // unrelated credential loading), so readiness must use the live
        // session's provider-aware catalog boundary exclusively.
        let session = self.ready_session(&owner.session_id).await.ok()?;
        let provider_catalog = session.model_catalog()?;
        let mut catalog = provider_catalog
            .refresh_provider(&owner.provider)
            .await
            .ok()?;
        let provider_ready = provider_catalog_is_ready(&catalog, &owner.provider);
        overlay_model_catalog_current(&mut catalog, selected, resolved);
        self.emit_many(
            &owner.client_id,
            &[EngineEvent::ModelsListed {
                meta: ack_meta(meta, &*self.clock),
                session_id: Some(owner.session_id.clone()),
                models: catalog.models,
                aliases: catalog.aliases,
                providers: catalog.providers,
                cached: catalog.cached,
                truncated: catalog.truncated,
            }],
        )
        .await;
        Some(provider_ready)
    }
}
