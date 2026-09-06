//! Provider context CPU work remains owned until its actual blocking worker exits.
use super::{
    context,
    context_memory::{self, ContextWorkingSet},
    provider_messages::persist_event,
    signals::TurnSignal,
};
use crate::engine::{
    AgentLoopError,
    pending_event::PendingEvent,
    projection::ContextSurgeryAction,
    recovery::{ConversationSource, HistoryRead},
    session::SessionActorConfig,
    task_ownership::ActorTasks,
};
use rw_context::AssembledContext;
use rw_tools::CancellationToken;
use rw_types::Turn;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub(super) enum Reservation {
    Fresh(HistoryRead<()>),
    Retained(ContextWorkingSet),
}
impl Reservation {
    fn admit(
        self,
        config: &SessionActorConfig,
        conversation: &[Turn],
    ) -> Result<ContextWorkingSet, AgentLoopError> {
        match self {
            Self::Fresh(reserved) => {
                context_memory::admit(reserved, config, conversation, &VecDeque::new())
            }
            Self::Retained(working) => {
                context_memory::readmit(working, config, conversation, &VecDeque::new())
            }
        }
    }
}
pub(super) struct Selection<'a> {
    pub conversation: &'a mut Vec<Turn>,
    pub sources: &'a [ConversationSource],
    pub surgery: &'a [ContextSurgeryAction],
    pub pruned: &'a mut BTreeMap<String, u64>,
}
pub(super) struct ProviderContext<'a> {
    pub config: &'a Arc<SessionActorConfig>,
    pub tasks: &'a ActorTasks,
    pub signals: &'a mpsc::UnboundedSender<TurnSignal>,
    pub cancellation: &'a CancellationToken,
}
impl ProviderContext<'_> {
    pub async fn assemble(
        &self,
        reservation: Reservation,
        selection: Selection<'_>,
        prune: bool,
    ) -> Result<(ContextWorkingSet, AssembledContext), AgentLoopError> {
        let conversation = Arc::new(Mutex::new(std::mem::take(selection.conversation)));
        let worker_conversation = Arc::clone(&conversation);
        let config = Arc::clone(self.config);
        let sources = selection.sources.to_vec();
        let surgery = selection.surgery.to_vec();
        let mut pruned = selection.pruned.clone();
        let task =
            self.tasks
                .spawn_blocking(Arc::clone(&config), self.cancellation.clone(), move || {
                    let conversation = worker_conversation
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let working = reservation.admit(&config, &conversation)?;
                    let events = if prune {
                        context::prune_plan(&working, &conversation, &sources, &surgery, &pruned)?
                    } else {
                        Vec::new()
                    };
                    for event in &events {
                        if let PendingEvent::ToolOutputPruned {
                            source,
                            reclaimed_tokens,
                        } = event
                        {
                            pruned.insert(source.key(), *reclaimed_tokens);
                        }
                    }
                    let assembled = context::assemble_session_context(
                        &config,
                        &working,
                        &conversation,
                        &sources,
                        &VecDeque::new(),
                        &surgery,
                        &pruned,
                    )?;
                    Ok::<_, AgentLoopError>((working, assembled, events, pruned))
                });
        let result = match task {
            Ok(task) => task
                .await
                .map_err(|error| {
                    AgentLoopError::EffectsUnsettled(format!(
                        "provider context worker failed: {error}"
                    ))
                })
                .and_then(std::convert::identity),
            Err(error) => Err(error),
        };
        // The worker has exited on every path. Restoring the owned source body
        // avoids a second full clone and preserves failure/completion metadata.
        *selection.conversation = std::mem::take(
            &mut *conversation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let (working, assembled, events, pruned) = result?;
        for event in events {
            persist_event(self.signals, event).await?;
        }
        *selection.pruned = pruned;
        Ok((working, assembled))
    }
}
