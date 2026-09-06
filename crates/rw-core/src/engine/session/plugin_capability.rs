use crate::engine::AgentLoopError;
use crate::engine::MAX_PLUGIN_ID_BYTES;
use crate::engine::MAX_PLUGIN_MESSAGE_BYTES;
use crate::engine::MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES;
use crate::engine::MAX_PLUGIN_NOTIFICATION_TITLE_BYTES;
use crate::engine::MessageDisposition;
use crate::engine::session::state::ActorCommand;
use std::fmt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Opaque, plugin-scoped machine capability for one session actor.
///
/// Its namespace is fixed by the host. Operations enter the actor and do not
/// acquire the client driver lease or bypass permission policy.
#[derive(Clone)]
pub struct PluginSessionCapability {
    pub(super) commands: mpsc::Sender<ActorCommand>,
    pub(super) plugin_id: String,
}

/// Weak host-owned factory used when publishing a fresh native generation.
/// It does not retain the public session handle or its shutdown lease.
#[derive(Clone)]
pub struct PluginSessionBinding {
    pub(super) commands: mpsc::WeakSender<ActorCommand>,
    pub(super) session_id: rw_types::SessionId,
}
impl PluginSessionBinding {
    #[must_use]
    pub fn session_id(&self) -> &rw_types::SessionId {
        &self.session_id
    }
    /// Bind one approved plugin namespace to the still-live actor.
    /// # Errors
    /// Rejects invalid namespace identities or a retired actor.
    pub fn bind(&self, plugin_id: &str) -> Result<PluginSessionCapability, AgentLoopError> {
        validate_plugin_id(plugin_id)?;
        let commands = self.commands.upgrade().ok_or(AgentLoopError::Closed)?;
        Ok(PluginSessionCapability {
            commands,
            plugin_id: plugin_id.to_owned(),
        })
    }
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
    /// Execute one declared host tool under the active command's immutable authority.
    /// # Errors
    /// Rejects invalid input, stale invocation, exhausted admission, policy or failed effect proof.
    pub async fn call_tool(
        &self,
        request: rw_types::extension_tools::ExtensionToolCall,
    ) -> Result<rw_types::extension_tools::ExtensionToolOutcome, AgentLoopError> {
        request
            .validate()
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.into()))?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::PluginToolCall { request, respond })
            .map_err(|_| {
                AgentLoopError::InvalidConfiguration("host tool admission unavailable".into())
            })?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Read one revision-bound page of prompt inventory metadata.
    /// # Errors
    /// Rejects invalid cursors, exhausted actor admission or closure.
    pub async fn read_context(
        &self,
        request: rw_types::extension_control::ExtensionContextRead,
    ) -> Result<rw_types::extension_control::ExtensionContextPage, AgentLoopError> {
        if let Some(id) = &request.after_item_id {
            rw_types::extension_control::validate_context_item_id(&id.0)
                .map_err(|error| AgentLoopError::InvalidConfiguration(error.into()))?;
        }
        let (respond, receive) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::PluginContextRead { request, respond })
            .map_err(|_| {
                AgentLoopError::InvalidConfiguration("plugin control admission unavailable".into())
            })?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Apply an explicit operation under this session's existing policy.
    /// # Errors
    /// Rejects invalid identities, exhausted admission, policy or persistence failure.
    pub async fn control(
        &self,
        origin: Option<rw_types::extension_invocation::ExtensionInvocationId>,
        control: rw_types::extension_control::ExtensionControl,
    ) -> Result<rw_types::extension_control::ExtensionControlOutcome, AgentLoopError> {
        control
            .validate()
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.into()))?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::PluginControl {
                origin,
                control,
                respond,
            })
            .map_err(|_| {
                AgentLoopError::InvalidConfiguration("plugin control admission unavailable".into())
            })?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Reads bounded operational state from the attached session actor.
    ///
    /// # Errors
    /// Rejects a closed actor.
    pub async fn query(
        &self,
    ) -> Result<rw_types::extension_contract::ExtensionSessionSnapshot, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginQuery { respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Reads only this plugin's canonical durable namespace.
    ///
    /// # Errors
    /// Rejects unavailable persistence or a closed actor.
    pub async fn read_state(
        &self,
    ) -> Result<rw_types::extension_contract::ExtensionStateSnapshot, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginStateRead {
                plugin_id: self.plugin_id.clone(),
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Commits a bounded compare-and-swap in this plugin's namespace.
    ///
    /// # Errors
    /// Rejects invalid state, exhausted admission, persistence failure or closure.
    pub async fn commit_state(
        &self,
        transaction: rw_types::extension_contract::ExtensionStateTransaction,
    ) -> Result<rw_types::extension_contract::ExtensionStateCommitOutcome, AgentLoopError> {
        rw_types::extension_contract::validate_state_transaction(&transaction)
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginStateCommit {
                plugin_id: self.plugin_id.clone(),
                transaction,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

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
        rw_types::session_state::validate_plugin_status(&self.plugin_id, &status)
            .map_err(|message| AgentLoopError::InvalidConfiguration(message.into()))?;
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
