//! Refusal for catalog screens invoked by a peer without an interactive client.
use super::{SessionCommandContext, SessionCommandOutput};
use async_trait::async_trait;
use rw_ext::{CommandExecutionError, CommandHandler, CommandInvocation};

/// Headless peers receive an explicit capability refusal for client screens.
/// Interactive clients open these catalog entries with their own UI owner.
pub(super) struct InteractiveClientCommand;
#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for InteractiveClientCommand {
    async fn execute(
        &self,
        _: &mut SessionCommandContext,
        _: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Err(CommandExecutionError::new(
            "interactive_client_required",
            "This command opens a screen in an interactive client.",
        ))
    }
}
