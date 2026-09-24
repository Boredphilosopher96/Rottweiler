//! Registry command for the same driver-scoped navigation contract exposed to extensions.
use super::{SessionCommandAction, SessionCommandContext, SessionCommandOutput};
use async_trait::async_trait;
use rw_ext::{CommandExecutionError, CommandHandler, CommandInvocation};
use rw_types::{SequenceId, SessionId, extension_control::SessionNavigationTarget};

pub(super) struct NavigateCommand;
#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for NavigateCommand {
    async fn execute(
        &self,
        _: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let mut arguments = invocation.arguments().split_whitespace();
        let target = match (arguments.next(), arguments.next(), arguments.next()) {
            (Some("session"), Some(id), None) => SessionNavigationTarget::Session {
                session_id: SessionId(id.into()),
            },
            (Some("sequence"), Some(sequence), None) => {
                let number = sequence.parse::<u64>().map_err(|_| usage())?;
                if number.to_string() != sequence {
                    return Err(usage());
                }
                SessionNavigationTarget::Transcript {
                    sequence: SequenceId(number),
                }
            }
            _ => return Err(usage()),
        };
        target.validate().map_err(|_| usage())?;
        Ok(SessionCommandOutput {
            message: "navigation requested".into(),
            action: SessionCommandAction::Navigate { target },
        })
    }
}
fn usage() -> CommandExecutionError {
    CommandExecutionError::new(
        "invalid_navigation",
        "usage: /goto session <id> | sequence <number>",
    )
}

/// Headless peers receive an explicit capability refusal for client navigation.
/// Interactive clients handle these registered actions with their own UI owner.
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
            "This navigation action requires an interactive client.",
        ))
    }
}
