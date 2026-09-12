//! Immutable command input captured before asynchronous execution.
use crate::engine::commands::SessionCommandContext;
use crate::engine::commands::render_permission_snapshot;
use crate::engine::commands::render_plan;
use std::sync::Arc;

use crate::engine::session::{ActorState, SessionActorConfig};
pub(super) fn capture(state: &ActorState, config: &SessionActorConfig) -> SessionCommandContext {
    SessionCommandContext {
        session_id: config.session_id.clone(),
        running: state.running.is_some() || state.initialization_running,
        queued_messages: state.queued.len(),
        mode: state.mode,
        mode_id: state.mode_id.clone(),
        modes: Arc::clone(&config.modes),
        permission_summary: render_permission_snapshot(&config.permissions.snapshot()),
        plan_summary: state
            .pending_plan
            .as_ref()
            .or(state.approved_plan.as_ref())
            .map_or_else(|| "no plan has been submitted".to_owned(), render_plan),
        command_summary: config
            .commands
            .descriptors()
            .map(|descriptor| {
                descriptor.argument_hint().map_or_else(
                    || format!("/{} — {}", descriptor.name(), descriptor.description()),
                    |hint| {
                        format!(
                            "/{} {} — {}",
                            descriptor.name(),
                            hint,
                            descriptor.description()
                        )
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}
