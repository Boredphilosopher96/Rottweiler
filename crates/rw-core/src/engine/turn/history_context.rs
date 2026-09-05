//! Context materialization is owned by a captured canonical source generation.
use crate::engine::AgentLoopError;
use crate::engine::recovery::{
    ConversationPage, HistoryMaterializationLimits, HistoryRead, SessionHistoryView,
};
use crate::engine::session::SessionActorConfig;
use rw_context::AssembledContext;
use rw_types::{SequenceId, Turn};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

pub(in crate::engine) struct CurrentContext {
    pub through: Option<SequenceId>,
    pub conversation: Vec<Turn>,
    pub pruned_tool_outputs: BTreeMap<String, u64>,
    pub assembled: AssembledContext,
}

pub(in crate::engine) async fn capture(
    config: &SessionActorConfig,
    expected_through: Option<SequenceId>,
) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
    let view = config.history.capture_history().await?;
    if view.through() != expected_through {
        return Err(AgentLoopError::Persistence(
            "canonical context prefix does not match the actor".into(),
        ));
    }
    Ok(view)
}

pub(in crate::engine) async fn read_view(
    view: &Arc<dyn SessionHistoryView>,
) -> Result<HistoryRead<ConversationPage>, AgentLoopError> {
    let end = view.conversation().turns;
    let page = view
        .conversation_page(0..end, HistoryMaterializationLimits::default())
        .await?;
    if page.range != (0..end) || page.has_more {
        return Err(AgentLoopError::InvalidConfiguration("conversation requires streamed compaction before a complete context can be materialized".into()));
    }
    Ok(page)
}

pub(in crate::engine) async fn read_current(
    config: &SessionActorConfig,
    expected_through: Option<SequenceId>,
) -> Result<HistoryRead<ConversationPage>, AgentLoopError> {
    read_view(&capture(config, expected_through).await?).await
}

pub(in crate::engine) async fn assemble_current(
    config: Arc<SessionActorConfig>,
    tasks: &crate::engine::task_ownership::ActorTasks,
    expected_through: Option<SequenceId>,
    queued: VecDeque<String>,
    include_dump: bool,
) -> Result<HistoryRead<CurrentContext>, AgentLoopError> {
    let view = capture(&config, expected_through).await?;
    assemble_view(config, tasks, view, queued, include_dump).await
}

pub(in crate::engine) async fn assemble_view(
    config: Arc<SessionActorConfig>,
    tasks: &crate::engine::task_ownership::ActorTasks,
    view: Arc<dyn SessionHistoryView>,
    queued: VecDeque<String>,
    include_dump: bool,
) -> Result<HistoryRead<CurrentContext>, AgentLoopError> {
    let through = view.through();
    let page = read_view(&view).await?;
    tasks
        .spawn_blocking(
            Arc::clone(&config),
            rw_tools::CancellationToken::default(),
            move || {
                let surgery = page
                    .context_actions
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>();
                let assembled = super::context::assemble_session_context(
                    &config,
                    &page.turns,
                    &queued,
                    &surgery,
                    &page.pruned_tool_outputs,
                    include_dump,
                )?;
                Ok(page.map(|page| CurrentContext {
                    through,
                    conversation: page.turns,
                    pruned_tool_outputs: page.pruned_tool_outputs,
                    assembled,
                }))
            },
        )?
        .await
        .map_err(|error| {
            AgentLoopError::EffectsUnsettled(format!("context assembly worker failed: {error}"))
        })?
}
