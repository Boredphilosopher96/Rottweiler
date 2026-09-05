use crate::engine::AgentLoopError;
use crate::engine::MAX_PLUGIN_ID_BYTES;
use crate::engine::MAX_PLUGIN_MESSAGE_BYTES;
use crate::engine::MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES;
use crate::engine::MAX_PLUGIN_NOTIFICATION_TITLE_BYTES;
use crate::engine::MAX_PLUGIN_STATUS_BYTES;
use crate::engine::MessageDisposition;
use crate::engine::session::state::ActorCommand;
use std::fmt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Opaque, plugin-scoped machine capability for one session actor.
///
/// This capability deliberately exposes only the three approved plugin push
/// operations. It cannot dispatch client commands, acquire the driver lease,
/// answer permissions, or interrupt a turn.
#[derive(Clone)]
pub struct PluginSessionCapability {
    pub(super) commands: mpsc::Sender<ActorCommand>,
    pub(super) plugin_id: String,
}

impl fmt::Debug for PluginSessionCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginSessionCapability")
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PluginSessionCapability {
    /// Injects one plain user message through normal actor sequencing.
    /// Slash-prefixed content remains a message and is never command-dispatched.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-bearing input and a closed actor.
    pub async fn inject_message(
        &self,
        content: impl Into<String>,
    ) -> Result<MessageDisposition, AgentLoopError> {
        let content = content.into();
        validate_plugin_text("injected message", &content, MAX_PLUGIN_MESSAGE_BYTES)?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginInjectMessage {
                plugin_id: self.plugin_id.clone(),
                content,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Publishes bounded session status text without taking the driver lease.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-bearing input, persistence failure,
    /// and a closed actor.
    pub async fn set_status(&self, status: impl Into<String>) -> Result<(), AgentLoopError> {
        let status = status.into();
        validate_plugin_text("plugin status", &status, MAX_PLUGIN_STATUS_BYTES)?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginSetStatus {
                plugin_id: self.plugin_id.clone(),
                status,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Publishes a bounded session-local UI notification.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-bearing input, persistence failure,
    /// and a closed actor.
    pub async fn notify(
        &self,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<(), AgentLoopError> {
        let title = title.into();
        let message = message.into();
        validate_plugin_text(
            "notification title",
            &title,
            MAX_PLUGIN_NOTIFICATION_TITLE_BYTES,
        )?;
        validate_plugin_text(
            "notification message",
            &message,
            MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES,
        )?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginNotify {
                plugin_id: self.plugin_id.clone(),
                title,
                message,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }
}

pub(in crate::engine) fn validate_plugin_text(
    label: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), AgentLoopError> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(AgentLoopError::InvalidConfiguration(format!(
            "{label} is empty, exceeds its byte limit, or contains control characters"
        )));
    }
    Ok(())
}

pub(in crate::engine) fn validate_plugin_id(plugin_id: &str) -> Result<(), AgentLoopError> {
    if plugin_id.is_empty()
        || plugin_id.len() > MAX_PLUGIN_ID_BYTES
        || !plugin_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(AgentLoopError::InvalidConfiguration(
            "plugin id must be a bounded canonical name".to_owned(),
        ));
    }
    Ok(())
}
