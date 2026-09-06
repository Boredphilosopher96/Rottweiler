//! Recovery refuses new work while the actor settles its previous physical tasks.
use crate::engine::{
    AgentLoopError,
    session::{ActorCommand, ActorState},
};
use rw_types::{CommandOutcome, EngineError, EngineErrorCategory};
use tokio::sync::oneshot;

fn reject_reply<T>(respond: oneshot::Sender<Result<T, AgentLoopError>>) {
    let _ = respond.send(Err(AgentLoopError::Persistence(
        "session is awaiting journal recovery".into(),
    )));
}

pub(super) fn reject(
    command: ActorCommand,
    state: &ActorState,
    events: &crate::engine::live_events::LiveEvents,
) {
    match command {
        ActorCommand::Protocol {
            command,
            respond,
            completion,
        } => {
            reject_protocol(&command, respond, completion, state, events);
        }
        ActorCommand::ChildControl {
            command,
            respond,
            completion,
            ..
        } => {
            reject_protocol(&command, respond, Some(completion), state, events);
        }
        ActorCommand::LiveState { respond } => reject_reply(respond),
        ActorCommand::ChildControls { respond } => reject_reply(respond),
        ActorCommand::Controls { respond } => reject_reply(respond),
        ActorCommand::UiCatalog { respond } => reject_reply(respond),
        ActorCommand::UiPanels { respond } => reject_reply(respond),
        ActorCommand::CompleteUserShell { respond, .. }
        | ActorCommand::RecordSubagentSpawned { respond, .. }
        | ActorCommand::RecordSubagentFinished { respond, .. }
        | ActorCommand::PluginSetStatus { respond, .. }
        | ActorCommand::PluginNotify { respond, .. } => reject_reply(respond),
        ActorCommand::PluginInjectMessage { respond, .. }
        | ActorCommand::SendMessage { respond, .. } => reject_reply(respond),
        ActorCommand::PluginContextRead { respond, .. } => reject_reply(respond),
        ActorCommand::PluginToolCall { respond, .. } => reject_reply(respond),
        ActorCommand::PluginControl { respond, .. } => reject_reply(respond),
        ActorCommand::PluginQuery { respond } => reject_reply(respond),
        ActorCommand::PluginStateRead { respond, .. } => reject_reply(respond),
        ActorCommand::PluginStateCommit { respond, .. } => reject_reply(respond),
        ActorCommand::PublishSubagentProgress(_) | ActorCommand::Snapshot { .. } => {}
        #[cfg(test)]
        ActorCommand::Interrupt { respond, .. } => {
            let _ = respond.send(false);
        }
    }
}

fn reject_protocol(
    command: &rw_types::ClientCommand,
    respond: oneshot::Sender<CommandOutcome>,
    completion: Option<
        oneshot::Sender<Result<crate::engine::session::ProtocolCompletion, AgentLoopError>>,
    >,
    state: &ActorState,
    events: &crate::engine::live_events::LiveEvents,
) {
    let outcome = CommandOutcome::Rejected {
        error: EngineError {
            category: EngineErrorCategory::Protocol,
            code: "session_recovering".into(),
            message: "session is awaiting journal recovery".into(),
            retryable: true,
            details: None,
        },
    };
    super::replies::send_ack(
        state,
        events,
        command.meta(),
        Some(state.session_id.clone()),
        outcome.clone(),
    );
    let _ = respond.send(outcome);
    if let Some(completion) = completion {
        reject_reply(completion);
    }
}
