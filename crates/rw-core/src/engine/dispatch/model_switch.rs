use crate::engine::AgentLoopError;
use crate::engine::RoutedEvent;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::PreparedModelSwitch;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::emit_batch;
use rw_types::ModelContextTransfer;
use std::sync::Arc;
use tokio::sync::broadcast;

pub(in crate::engine) async fn commit_prepared_model_switch(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    prepared: PreparedModelSwitch,
    clear_context: bool,
) -> Result<(), AgentLoopError> {
    let mut durable = Vec::with_capacity(if clear_context { 2 } else { 1 });
    if clear_context {
        durable.push(PendingEvent::ModelContextCleared {
            strategy: ModelContextTransfer::StartWithoutContext,
        });
    }
    durable.push(PendingEvent::ModelChanged {
        model: prepared.model.clone(),
        provider: prepared.provider.clone(),
        thinking: prepared.thinking,
    });
    let result = emit_batch(state, events, &config.event_sink, durable)
        .await
        .map(|_| ());
    if result.is_ok() {
        if clear_context {
            state.clear_conversation_except_system();
        }
        config.model.commit_prepared_model(&prepared.model.0);
        state.model_alias = prepared.model.0;
        state.provider = prepared.provider;
        state.thinking = prepared.thinking;
    } else {
        config.model.discard_prepared_model(&prepared.model.0);
    }
    result
}

/// Selection emits the ordinary context-transfer question when a decision is needed.
pub(super) async fn request_model_selection(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    model: rw_types::ModelAlias,
    provider: Option<String>,
) -> Result<Option<rw_types::QuestionId>, AgentLoopError> {
    use crate::engine::{model_switch_question, session::PendingModelSwitch, turn::emit};
    use rw_types::QuestionId;
    let thinking = config.model.thinking_for_model(&model.0, state.thinking);
    let prepared = PreparedModelSwitch {
        model: model.clone(),
        provider: provider.clone(),
        thinking,
    };
    let has_prior_context = state.has_conversation_context();
    if has_prior_context && (state.model_alias != model.0 || state.provider != provider) {
        let question_id = QuestionId(format!("model-switch-{}", state.next_question));
        state.next_question = state.next_question.saturating_add(1);
        let question = model_switch_question(question_id.clone(), model.clone(), provider.clone());
        rw_types::question_admission::validate_question(&question)
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.into()))?;
        if state.pending_questions.len() + state.pending_model_switches.len()
            >= rw_types::question_admission::MAX_PENDING_QUESTION_REQUESTS
        {
            return Err(AgentLoopError::InvalidConfiguration(
                "pending question admission is full".into(),
            ));
        }
        let result = emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::QuestionAsked {
                turn: state.completed_turns,
                question_id: question_id.clone(),
                question: question.clone(),
            },
        )
        .await
        .map(|_| ());
        if result.is_ok() {
            state.pending_model_switches.insert(
                question_id.0.clone(),
                PendingModelSwitch {
                    question,
                    turn: state.completed_turns,
                    model,
                    provider,
                },
            );
        }
        result.map(|()| Some(question_id))
    } else {
        commit_prepared_model_switch(state, config, events, prepared, false)
            .await
            .map(|()| None)
    }
}
