//! Persist queued model selections at the actor's completion boundary.
pub(super) struct HostedModelPreferences {
    pub(super) loader: rw_store::config::ConfigLoader,
}

#[async_trait::async_trait]
impl rw_core::ModelSelectionPreferences for HostedModelPreferences {
    async fn persist(&self, model: &str) -> Result<(), rw_core::AgentLoopError> {
        let loader = self.loader.clone();
        let model = model.to_owned();
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            loader.persist_tui_project_model(&model)
        })
        .await
        .map_err(|_| rw_core::AgentLoopError::Persistence("model preference worker failed".into()))?
        .map(|_| ())
        .map_err(|error| rw_core::AgentLoopError::Persistence(error.to_string()))
    }
}
