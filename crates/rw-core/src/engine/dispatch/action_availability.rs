//! One readiness policy for actor admission and client discovery.
use crate::engine::session::{ActorState, SessionActorConfig};
use rw_types::{ClientCommand, SessionActionAvailability, SessionActionKind};

#[derive(Default)]
enum ActionPhase {
    #[default]
    Idle,
    Turn,
    Shell,
    Command,
    ModelPreparation,
    Closing,
    Recovery,
}

#[derive(Default)]
pub(super) struct ActionState {
    phase: ActionPhase,
    model_question: bool,
    queue_depth: usize,
    child_active: bool,
}

impl ActionState {
    pub(super) fn from_actor(state: &ActorState, config: &SessionActorConfig) -> Self {
        Self::from_actor_with_origin(state, config, None)
    }

    pub(super) fn from_actor_with_origin(
        state: &ActorState,
        config: &SessionActorConfig,
        origin: Option<&rw_types::extension_invocation::ExtensionInvocationId>,
    ) -> Self {
        let own_command =
            state
                .pending_command
                .as_ref()
                .zip(origin)
                .is_some_and(|(pending, origin)| {
                    pending.allows(origin, config, state.control.driver().as_ref())
                });
        let phase = if state.closing {
            ActionPhase::Closing
        } else if state.poisoned || state.recovery_requested || state.unsettled.is_some() {
            ActionPhase::Recovery
        } else if state.pending_model_preparation.is_some() {
            ActionPhase::ModelPreparation
        } else if state.pending_command.is_some() && !own_command {
            ActionPhase::Command
        } else if state.running.is_some() || state.initialization_running {
            ActionPhase::Turn
        } else if state.active_shell.is_some() {
            ActionPhase::Shell
        } else {
            ActionPhase::Idle
        };
        Self {
            phase,
            model_question: !state.pending_model_switches.is_empty(),
            queue_depth: state.deferred_controls.len(),
            child_active: config.tools.session_activity(&state.session_id).is_some(),
        }
    }

    pub(super) fn unavailable(
        &self,
        action: SessionActionKind,
    ) -> Option<(&'static str, &'static str)> {
        match self.phase {
            ActionPhase::Closing => Some(("session_closing", "This session is closing.")),
            ActionPhase::Recovery => Some((
                "session_requires_recovery",
                "Wait for this session to recover.",
            )),
            ActionPhase::ModelPreparation => Some((
                "model_preparation_busy",
                "Wait for model preparation to finish.",
            )),
            ActionPhase::Command => {
                Some(("command_busy", "Wait for the current command to finish."))
            }
            ActionPhase::Turn => Some((
                if action == SessionActionKind::Compact {
                    "turn_running"
                } else {
                    "session_not_idle"
                },
                "Stop the current turn or wait for it to finish.",
            )),
            ActionPhase::Shell => Some((
                "session_not_idle",
                "Finish the foreground terminal command first.",
            )),
            ActionPhase::Idle
                if action == SessionActionKind::SwitchModel && self.model_question =>
            {
                Some((
                    "model_switch_pending",
                    "Choose how to transfer context for the pending model switch first.",
                ))
            }
            ActionPhase::Idle if !queueable_action(action) && self.child_active => Some((
                "session_not_idle",
                "Wait for the active child agent or background command to finish.",
            )),
            ActionPhase::Idle
                if !queueable_action(action) && (self.model_question || self.queue_depth > 0) =>
            {
                Some((
                    "session_not_idle",
                    "Finish pending context choices and queued controls first.",
                ))
            }
            ActionPhase::Idle => None,
        }
    }

    pub(super) fn projection(&self) -> Vec<SessionActionAvailability> {
        [
            SessionActionKind::SwitchModel,
            SessionActionKind::SwitchMode,
            SessionActionKind::Compact,
            SessionActionKind::Rewind,
            SessionActionKind::Review,
            SessionActionKind::Fork,
            SessionActionKind::AddWorkspaceRoot,
            SessionActionKind::MutateContext,
        ]
        .into_iter()
        .map(|action| {
            let queueable = queueable_action(action)
                && !matches!(
                    self.phase,
                    ActionPhase::Closing | ActionPhase::Recovery | ActionPhase::Shell
                );
            let queued = queueable
                && (self.queue_depth > 0
                    || self.model_question
                    || !matches!(self.phase, ActionPhase::Idle));
            let unavailable_reason = if queued
                && self.queue_depth >= rw_types::MAX_QUEUED_SESSION_CONTROLS
            {
                Some("The control queue is full. Wait for a queued control to finish.".to_owned())
            } else if queued {
                None
            } else {
                self.unavailable(action)
                    .map(|(_, reason)| reason.to_owned())
            };
            SessionActionAvailability {
                action,
                queued,
                unavailable_reason,
            }
        })
        .collect()
    }
}

pub(super) fn command_action(command: &ClientCommand) -> Option<SessionActionKind> {
    match command {
        ClientCommand::SwitchModel { .. } => Some(SessionActionKind::SwitchModel),
        ClientCommand::SwitchMode { .. } => Some(SessionActionKind::SwitchMode),
        ClientCommand::Compact { .. } => Some(SessionActionKind::Compact),
        ClientCommand::GetSessionReview { .. } | ClientCommand::ReviewFile { .. } => {
            Some(SessionActionKind::Review)
        }
        ClientCommand::PinContext { .. } | ClientCommand::EvictContext { .. } => {
            Some(SessionActionKind::MutateContext)
        }
        _ => None,
    }
}

pub(super) fn slash_action(name: &str) -> Option<SessionActionKind> {
    match name {
        "rewind" => Some(SessionActionKind::Rewind),
        "review" => Some(SessionActionKind::Review),
        "dirs" => Some(SessionActionKind::AddWorkspaceRoot),
        _ => None,
    }
}

fn queueable_action(action: SessionActionKind) -> bool {
    matches!(
        action,
        SessionActionKind::SwitchModel | SessionActionKind::SwitchMode | SessionActionKind::Compact
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_wait_for_turn_shell_and_generation_owners() {
        for phase in [
            ActionPhase::Turn,
            ActionPhase::Shell,
            ActionPhase::Command,
            ActionPhase::ModelPreparation,
            ActionPhase::Closing,
            ActionPhase::Recovery,
        ] {
            let state = ActionState {
                phase,
                ..Default::default()
            };
            assert!(
                state
                    .projection()
                    .iter()
                    .all(|entry| entry.queued || entry.unavailable_reason.is_some())
            );
        }
        assert!(
            ActionState::default()
                .projection()
                .iter()
                .all(|entry| entry.unavailable_reason.is_none())
        );
        let pending_choice = ActionState {
            model_question: true,
            ..Default::default()
        };
        assert_eq!(
            pending_choice
                .unavailable(SessionActionKind::SwitchModel)
                .map(|(code, _)| code),
            Some("model_switch_pending")
        );
        assert!(
            pending_choice
                .unavailable(SessionActionKind::SwitchMode)
                .is_none()
        );
        assert!(
            pending_choice
                .unavailable(SessionActionKind::Compact)
                .is_none()
        );
    }
    #[test]
    fn idle_only_actions_share_child_and_shell_refusals_without_queueing() {
        for state in [
            ActionState {
                child_active: true,
                ..Default::default()
            },
            ActionState {
                phase: ActionPhase::Shell,
                ..Default::default()
            },
            ActionState {
                queue_depth: 1,
                ..Default::default()
            },
        ] {
            for entry in state
                .projection()
                .into_iter()
                .filter(|entry| !queueable_action(entry.action))
            {
                assert!(!entry.queued);
                assert_eq!(
                    entry.unavailable_reason.as_deref(),
                    state.unavailable(entry.action).map(|(_, reason)| reason)
                );
                assert!(entry.unavailable_reason.is_some());
            }
        }
    }
}
