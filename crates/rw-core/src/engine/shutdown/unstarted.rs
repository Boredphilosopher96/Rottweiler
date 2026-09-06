//! Retire a prepared configuration rejected before actor publication.
use super::{SHUTDOWN_PROOF_TIMEOUT, retain_unproven};
use crate::SessionActorConfig;
use futures_util::FutureExt;
use std::{future::Future, sync::Arc};

pub(crate) async fn settle_unstarted(config: SessionActorConfig) -> Result<(), String> {
    let owner = Arc::new(config);
    let result = tokio::time::timeout(SHUTDOWN_PROOF_TIMEOUT, settle(&owner))
        .await
        .unwrap_or_else(|_| Err("unpublished child settlement deadline exceeded".into()));
    if result.is_err() {
        tokio::spawn(retain_unproven(owner));
    }
    result
}
async fn proof<E: std::fmt::Display>(
    work: impl Future<Output = Result<(), E>>,
    failure: &mut Option<String>,
) {
    let result = std::panic::AssertUnwindSafe(work).catch_unwind().await;
    let error = match result {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error.to_string(),
        Err(_) => "unpublished child effect owner panicked".into(),
    };
    failure.get_or_insert(error);
}
async fn settle(config: &SessionActorConfig) -> Result<(), String> {
    let mut failure = None;
    proof(config.model.settle_effects(), &mut failure).await;
    for descriptor in config.tools.descriptors() {
        if let Some(tool) = config.tools.resolve(&descriptor.name) {
            proof(tool.settle_effects(), &mut failure).await;
        }
    }
    for event in [
        rw_ext::HookEvent::SessionStart,
        rw_ext::HookEvent::SessionEnd,
        rw_ext::HookEvent::UserPromptSubmit,
        rw_ext::HookEvent::PreTool,
        rw_ext::HookEvent::PostTool,
        rw_ext::HookEvent::PreCompact,
        rw_ext::HookEvent::TurnEnd,
        rw_ext::HookEvent::PermissionCheck,
    ] {
        proof(config.hooks.settle_effects(event), &mut failure).await;
    }
    proof(config.tools.end_session(&config.session_id), &mut failure).await;
    proof(config.event_sink.settle_effects(), &mut failure).await;
    proof(config.checkpoints.settle_effects(), &mut failure).await;
    proof(config.extension_development.shutdown(), &mut failure).await;
    proof(config.resources.shutdown(), &mut failure).await;
    failure.map_or(Ok(()), Err)
}
