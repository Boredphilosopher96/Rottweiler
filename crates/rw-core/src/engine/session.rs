mod actor;
mod child_progress;
mod config;
mod control;
mod conversation_summary;
pub(in crate::engine) use conversation_summary::ConversationSummary;
mod handle;
mod live_state;
mod plugin_capability;
mod recovery;
mod state;
mod subscription;
pub use actor::SessionActor;
pub use config::SessionActorConfig;
pub use config::StartupNotification;
pub use handle::SessionHandle;
pub(super) use plugin_capability::validate_plugin_id;
pub(super) use plugin_capability::validate_plugin_text;
pub use plugin_capability::{PluginSessionBinding, PluginSessionCapability};
pub(super) use recovery::recover_actor_from_journal;
pub(super) use state::ActorCommand;
pub(super) use state::ActorState;
pub(super) use state::PendingApproval;
pub(super) use state::PendingModelSwitch;
pub(super) use state::PendingQuestion;
pub(super) use state::PrecommittedAnswer;
pub(super) use state::PreparedModelSwitch;
pub(super) use state::ProtocolCompletion;
pub use subscription::SessionSubscription;
pub(super) use subscription::validate_gap;

#[cfg(test)]
pub(super) use recovery::interrupted_tool_recovery_events;

pub(super) use control::SessionControl;
