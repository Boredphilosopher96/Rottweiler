use crate::engine::session::{ActorState, SessionActorConfig};
use rw_types::{ClientCommand, CommandOutcome, RewindTarget};

use super::replies::protocol_rejection;

pub(super) async fn rejection(
    command: &ClientCommand,
    state: &ActorState,
    config: &SessionActorConfig,
) -> Option<CommandOutcome> {
    let (turn_id, code, is_rewind) = match command {
        ClientCommand::Rewind {
            target: RewindTarget::Turn { turn_id },
            ..
        } => (turn_id, "invalid_rewind_target", true),
        ClientCommand::DumpPrompt {
            turn_id: Some(turn_id),
            ..
        } => (turn_id, "unknown_prompt_turn", false),
        _ => return None,
    };
    let turn = match crate::engine::projection::parse_turn_id(turn_id) {
        Ok(turn) => turn,
        Err(error) => {
            return Some(protocol_rejection(
                if is_rewind { "invalid_turn_id" } else { code },
                error.to_string(),
            ));
        }
    };
    if is_rewind {
        if state.running.is_some() || config.tools.session_activity(&state.session_id).is_some() {
            return Some(protocol_rejection(code, "rewind requires an idle session"));
        }
        if state.pending_rewind.as_ref().map(|pending| pending.0) == Some(turn) {
            return None;
        }
    } else if state.running.as_ref().map(|running| running.id) == Some(turn) {
        return None;
    }
    match config.event_sink.completed_turn(turn).await {
        Ok(Some(_)) => None,
        Ok(None) => Some(protocol_rejection(
            code,
            "turn is not a currently completed target",
        )),
        Err(error) => Some(protocol_rejection(
            "completed_turn_unavailable",
            error.to_string(),
        )),
    }
}
