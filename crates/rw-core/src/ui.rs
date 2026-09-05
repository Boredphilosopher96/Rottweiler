//! Headless presentation and action authority shared by all clients.
use crate::{AgentLoopError, SessionCommandContext, SessionCommandOutput};
use rw_ext::BoundCommand;
use rw_types::extension_ui::{UiActionRequest, UiCatalog, UiPanels, UiPresentation};

pub type BoundUiCommand = BoundCommand<SessionCommandContext, SessionCommandOutput>;

/// A session's live declarative contribution registry. Reads are synchronous,
/// bounded snapshots; they never invoke extension code. The command channel owns
/// read allocation admission and driver authority before calling this boundary.
pub trait UiRegistry: Send + Sync {
    fn catalog(&self) -> Result<UiCatalog, AgentLoopError>;
    fn panels(&self) -> Result<UiPanels, AgentLoopError>;

    /// Resolves against the exact live generation and host-owned action data.
    /// For tools, `tool` must come from the canonical invocation query at the
    /// actor's exact committed prefix. Panels use the registry's live revision.
    fn resolve_action(
        &self,
        request: &UiActionRequest,
        tool: Option<&UiPresentation>,
    ) -> Result<BoundUiCommand, AgentLoopError>;
}

/// Explicitly configured sessions without declarative contributions.
pub struct EmptyUiRegistry;
impl UiRegistry for EmptyUiRegistry {
    fn catalog(&self) -> Result<UiCatalog, AgentLoopError> {
        Ok(UiCatalog {
            entries: Vec::new(),
        })
    }
    fn panels(&self) -> Result<UiPanels, AgentLoopError> {
        Ok(UiPanels { panels: Vec::new() })
    }
    fn resolve_action(
        &self,
        _request: &UiActionRequest,
        _tool: Option<&UiPresentation>,
    ) -> Result<BoundUiCommand, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "UI contribution is unavailable".into(),
        ))
    }
}
