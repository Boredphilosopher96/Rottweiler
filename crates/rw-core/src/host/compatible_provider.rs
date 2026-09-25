use super::{Arc, CommandOutcome, EngineEvent, EngineHost, HostError, SessionId, ack_meta};
use rw_types::{CommandMeta, CompatibleProviderAuth, CompatibleProviderSetup, ProviderAuthKind};

impl EngineHost {
    pub(super) async fn configure_compatible_provider(
        &self,
        meta: CommandMeta,
        session_id: SessionId,
        configuration: CompatibleProviderSetup,
    ) -> Result<(CommandOutcome, Option<SessionId>, Vec<EngineEvent>), HostError> {
        configuration
            .validate()
            .map_err(|error| HostError::Protocol(error.into()))?;
        let session = self.ready_session(&session_id).await?;
        let queries = Arc::clone(&self.queries);
        let provider_mutation = Arc::clone(&self.provider_mutation);
        let actor = meta.client_id.clone();
        let provider = configuration.provider.clone();
        let auth_kind = match configuration.auth {
            CompatibleProviderAuth::ApiKey => ProviderAuthKind::ApiKey,
            CompatibleProviderAuth::None => ProviderAuthKind::None,
        };
        tokio::spawn(async move {
            let _provider_mutation = provider_mutation.lock_owned().await;
            let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
            if session.handle().snapshot().await?.driver_client_id.as_ref() != Some(&actor) {
                return Err(HostError::Protocol(
                    "only the current driver may configure providers".into(),
                ));
            }
            queries.configure_compatible_provider(&configuration).await
        })
        .await
        .map_err(|_| HostError::Query("provider setup task failed".into()))??;
        Ok((
            CommandOutcome::Accepted {},
            Some(session_id.clone()),
            vec![EngineEvent::ProviderConfigured {
                meta: ack_meta(&meta, &*self.clock),
                session_id,
                provider,
                auth_kind,
            }],
        ))
    }
}
