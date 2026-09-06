//! Dormant discovery reads canonical controls without composing executable resources.
use super::{
    extension_discovery::discover_runtime_extensions_derived, tool_composition::trusted_lsp_roots,
    workspace_roots::RuntimeWorkspaceRootController,
};
#[cfg(test)]
use crate::journal_service::JournalService;
use rw_core::{AgentLoopError, DormantChildControls, recovery::CanonicalRecovery};
use rw_ext::ModeRegistry;
use rw_types::SessionId;
use std::{path::Path, sync::Arc};

impl RuntimeWorkspaceRootController {
    pub(super) async fn dormant_controls(
        self: &Arc<Self>,
        session: &SessionId,
        workspace: &Path,
    ) -> Result<DormantChildControls, AgentLoopError> {
        let owner = self.clone();
        let workspace = workspace.to_path_buf();
        let session = session.clone();
        let journals = self.journal_service.clone();
        let admission = journals.admit_read().map_err(failure)?;
        let allowance = journals.retain_history().await?;
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let _allowance = allowance;
            let roots = [workspace.clone()];
            let trusted =
                trusted_lsp_roots(&roots, &owner.trust_store_path, owner.dangerously_trust)
                    .map_err(failure)?;
            let catalog = discover_runtime_extensions_derived(
                &workspace,
                &owner.extension_user_home,
                &owner.extension_user_rottweiler,
                trusted[0],
            );
            let modes = rw_ext::compose_mode_registry(&catalog).map_err(failure)?;
            let order = journals
                .routing_projection_order(&session.0)
                .map_err(failure)?;
            let _order = order
                .lock()
                .map_err(|_| failure("child source owner poisoned"))?;
            let lease = admission.capture(&session.0).map_err(failure)?;
            summarize(&lease.view, &session, &modes)
        })
        .await
        .map_err(failure)?
    }
}

pub(super) fn summarize(
    source: &rw_store::session::journal::JournalReadView,
    session: &SessionId,
    modes: &ModeRegistry,
) -> Result<DormantChildControls, AgentLoopError> {
    let mut recovery = CanonicalRecovery::for_control_discovery(source, modes).map_err(failure)?;
    while recovery.advance(source, modes).map_err(failure)?.has_more {}
    DormantChildControls::from_head(session, &recovery.head().map_err(failure)?)
}

#[cfg(test)]
pub(super) async fn fixture_controls(
    journals: Arc<JournalService>,
    session: SessionId,
) -> Result<DormantChildControls, AgentLoopError> {
    let admission = journals.admit_read().map_err(failure)?;
    let allowance = journals.retain_history().await?;
    rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        let _allowance = allowance;
        let lease = admission.capture(&session.0).map_err(failure)?;
        summarize(
            &lease.view,
            &session,
            &ModeRegistry::builtins().map_err(failure)?,
        )
    })
    .await
    .map_err(failure)?
}
fn failure(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(error.to_string())
}
