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
    pub sources: Vec<crate::engine::recovery::ConversationSource>,
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

pub(in crate::engine) async fn assemble_view(
    config: Arc<SessionActorConfig>,
    tasks: &crate::engine::task_ownership::ActorTasks,
    view: Arc<dyn SessionHistoryView>,
    queued: VecDeque<String>,
) -> Result<HistoryRead<CurrentContext>, AgentLoopError> {
    let through = view.through();
    let page = read_view(&view).await?;
    let reserved = view.reserve_working_set()?;
    tasks
        .spawn_blocking(
            Arc::clone(&config),
            rw_tools::CancellationToken::default(),
            rw_resources::ResourceClass::Cpu,
            move || {
                let working = super::context_memory::admit(
                    reserved,
                    &config,
                    &page.turns,
                    &page.sources,
                    &queued,
                )?;
                let surgery = page
                    .context_actions
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>();
                let assembled = super::context::assemble_session_context(
                    &config,
                    &working,
                    &page.turns,
                    &page.sources,
                    &queued,
                    &surgery,
                    &page.pruned_tool_outputs,
                )?;
                Ok(page.retain(working).map(|page| CurrentContext {
                    through,
                    conversation: page.turns,
                    sources: page.sources,
                    pruned_tool_outputs: page.pruned_tool_outputs,
                    assembled,
                }))
            },
        )
        .await?
        .await
        .map_err(|error| {
            AgentLoopError::EffectsUnsettled(format!("context assembly worker failed: {error}"))
        })?
}

/// Exact selectors for the bounded request-local conversation. The provider loop
/// has already acknowledged every body commit before requesting these identities.
pub(super) async fn current_sources(
    config: &SessionActorConfig,
    turns: usize,
) -> Result<HistoryRead<Vec<crate::engine::recovery::ConversationSource>>, AgentLoopError> {
    let view = config.history.capture_history().await?;
    if view.conversation().turns != turns as u64 {
        return Err(AgentLoopError::Persistence(
            "request conversation does not match canonical source".into(),
        ));
    }
    view.conversation_sources(0..turns as u64).await
}

pub(super) async fn reserve_working(
    config: &SessionActorConfig,
) -> Result<Box<dyn crate::engine::recovery::HistoryWorkingAllowance>, AgentLoopError> {
    config
        .history
        .capture_history()
        .await?
        .reserve_working_set()
}
