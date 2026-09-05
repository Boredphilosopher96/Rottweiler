//! Headless presentation and action authority shared by all clients.
use crate::{AgentLoopError, SessionCommandContext, SessionCommandOutput};
use rw_ext::BoundCommand;
use rw_types::extension_ui::{UiActionRequest, UiCatalog, UiPanels, UiPresentation};

pub type BoundUiCommand = BoundCommand<SessionCommandContext, SessionCommandOutput>;

/// A session's live declarative contribution registry. Reads are synchronous,
/// bounded snapshots; they never invoke extension code. The command channel owns
/// read allocation admission and driver authority before calling this boundary.
pub trait UiRegistry: Send + Sync {
    fn owns(&self, owner: &rw_types::extension_ui::UiContributionOwner) -> bool;
    /// Read the bounded approved catalog without activating code.
    /// # Errors
    /// Rejects unavailable or oversized registry snapshots.
    fn catalog(&self) -> Result<UiCatalog, AgentLoopError>;
    /// Read the bounded live panel state.
    /// # Errors
    /// Rejects unavailable or oversized registry snapshots.
    fn panels(&self) -> Result<UiPanels, AgentLoopError>;

    /// Resolves against the exact live generation and host-owned action data.
    /// For tools, `tool` must come from the canonical invocation query at the
    /// actor's exact committed prefix. Panels use the registry's live revision.
    /// # Errors
    /// Rejects stale generation, source, revision or undeclared action identities.
    fn resolve_action(
        &self,
        request: &UiActionRequest,
        tool: Option<&UiPresentation>,
    ) -> Result<BoundUiCommand, AgentLoopError>;
}

/// Explicitly configured sessions without declarative contributions.
pub struct EmptyUiRegistry;
impl UiRegistry for EmptyUiRegistry {
    fn owns(&self, _owner: &rw_types::extension_ui::UiContributionOwner) -> bool {
        false
    }
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

/// Session-bound canonical source authority for a completed tool invocation.
#[async_trait::async_trait]
pub trait UiToolSource: Send + Sync {
    async fn presentation(
        &self,
        invocation: &rw_types::ToolInvocationId,
        expected_through: Option<rw_types::SequenceId>,
    ) -> Result<Option<UiPresentation>, AgentLoopError>;
}
pub struct UnavailableUiToolSource;
#[async_trait::async_trait]
impl UiToolSource for UnavailableUiToolSource {
    async fn presentation(
        &self,
        _invocation: &rw_types::ToolInvocationId,
        _expected_through: Option<rw_types::SequenceId>,
    ) -> Result<Option<UiPresentation>, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "tool presentation source is unavailable".into(),
        ))
    }
}

/// A configured base and one temporary development generation share aggregate
/// display limits. Handler resolution stays with the exact owning registry.
pub struct CombinedUiRegistry {
    base: std::sync::Arc<dyn UiRegistry>,
    added: std::sync::Arc<dyn UiRegistry>,
}
impl CombinedUiRegistry {
    /// # Errors
    /// Rejects duplicate identities and aggregate count/byte limits before publication.
    pub fn new(
        base: std::sync::Arc<dyn UiRegistry>,
        added: std::sync::Arc<dyn UiRegistry>,
    ) -> Result<Self, AgentLoopError> {
        let registry = Self { base, added };
        registry
            .catalog()?
            .validate()
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        registry
            .panels()?
            .validate()
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        Ok(registry)
    }
}
impl UiRegistry for CombinedUiRegistry {
    fn owns(&self, owner: &rw_types::extension_ui::UiContributionOwner) -> bool {
        self.base.owns(owner) || self.added.owns(owner)
    }
    fn catalog(&self) -> Result<UiCatalog, AgentLoopError> {
        let mut catalog = self.base.catalog()?;
        catalog.entries.extend(self.added.catalog()?.entries);
        catalog
            .validate()
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        Ok(catalog)
    }
    fn panels(&self) -> Result<UiPanels, AgentLoopError> {
        let mut panels = self.base.panels()?;
        panels.panels.extend(self.added.panels()?.panels);
        panels
            .validate()
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        Ok(panels)
    }
    fn resolve_action(
        &self,
        request: &UiActionRequest,
        tool: Option<&UiPresentation>,
    ) -> Result<BoundUiCommand, AgentLoopError> {
        if self.base.owns(&request.owner) {
            self.base.resolve_action(request, tool)
        } else {
            self.added.resolve_action(request, tool)
        }
    }
}
