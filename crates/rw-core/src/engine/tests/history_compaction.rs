#![cfg(test)]
use super::fixtures::{
    history,
    models::M3Model,
    support::{collect_turn, config, stop_script, text_turn},
};
use crate::engine::{
    AgentLoopError, AgentTurnStatus, SessionActor, builtin_hook_dispatcher,
    pending_event::PendingEvent,
    recovery::{
        ConversationCut, ConversationPage, HistoryMaterializationLimits, HistoryRead,
        RecoveryBootstrap, SessionHistory, SessionHistoryView,
    },
};
use async_trait::async_trait;
use rw_providers::{ProviderError, ProviderErrorKind};
use rw_tools::ToolRegistry;
use rw_types::{PromptDump, Role, SequenceId, config::PermissionDecision};
use std::{ops::Range, sync::Arc};

struct SmallPages(Arc<dyn SessionHistory>);
struct SmallView(Arc<dyn SessionHistoryView>);
#[async_trait]
impl SessionHistory for SmallPages {
    async fn capture_history(&self) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
        Ok(Arc::new(SmallView(self.0.capture_history().await?)))
    }
}
#[async_trait]
impl SessionHistoryView for SmallView {
    fn through(&self) -> Option<SequenceId> {
        self.0.through()
    }
    fn conversation(&self) -> ConversationCut {
        self.0.conversation()
    }
    async fn conversation_sources(
        &self,
        range: Range<u64>,
    ) -> Result<HistoryRead<Vec<crate::engine::recovery::ConversationSource>>, AgentLoopError> {
        self.0.conversation_sources(range).await
    }
    async fn source_turn(
        &self,
        sequence: SequenceId,
    ) -> Result<
        HistoryRead<Option<(u64, crate::engine::recovery::ConversationSource)>>,
        AgentLoopError,
    > {
        self.0.source_turn(sequence).await
    }
    fn reserve_working_set(&self) -> Result<HistoryRead<()>, AgentLoopError> {
        self.0.reserve_working_set()
    }
    async fn bootstrap(&self) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError> {
        self.0.bootstrap().await
    }
    async fn recovery_at_completed_turn(
        &self,
        turn: u64,
    ) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError> {
        self.0.recovery_at_completed_turn(turn).await
    }
    async fn prompt_at_turn(
        &self,
        turn: u64,
    ) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
        Ok(Arc::new(Self(self.0.prompt_at_turn(turn).await?)))
    }
    fn verify_prompt(&self, turn: u64, dump: &PromptDump) -> Result<(), AgentLoopError> {
        self.0.verify_prompt(turn, dump)
    }
    async fn conversation_page(
        &self,
        range: Range<u64>,
        mut limits: HistoryMaterializationLimits,
    ) -> Result<HistoryRead<ConversationPage>, AgentLoopError> {
        limits.max_turns = limits.max_turns.min(8);
        self.0.conversation_page(range, limits).await
    }
}

async fn fixture(
    root: &std::path::Path,
    model: Arc<M3Model>,
    mask: bool,
) -> (crate::engine::SessionHandle, Arc<dyn SessionHistory>) {
    let mut config = config(
        root,
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    config.recovered.conversation = (0..18)
        .map(|index| {
            text_turn(
                if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                format!("source-{index:02}"),
            )
        })
        .collect();
    if mask {
        config.recovered.conversation[1].blocks = vec![rw_types::Block::ToolResult {
            id: rw_types::ToolCallId("masked-call".into()),
            output: rw_types::ToolOutput::Text {
                text: "PRUNED_SECRET".into(),
            },
            is_error: false,
        }];
        config.recovered.conversation[2] = text_turn(Role::User, "EVICTED_SECRET");
        config
            .recovered
            .pruned_tool_outputs
            .insert("1:0".into(), 42);
        config
            .recovered
            .context_surgery
            .push(crate::engine::projection::ContextSurgeryAction {
                item_id: rw_types::ContextItemId("conversation:2".into()),
                pinned: false,
                effective_after_agent_turn: 1,
            });
    }
    let mut config = history::bind(config).await.expect("canonical fixture");
    let source = Arc::clone(&config.history);
    config.history = Arc::new(SmallPages(Arc::clone(&source)));
    (SessionActor::spawn(config).expect("actor"), source)
}

#[tokio::test]
async fn overflow_consumes_every_page_and_keeps_new_input_after_atomic_summary() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(M3Model::new([
        stop_script("summary-one", &[]),
        stop_script("summary-two", &[]),
        stop_script("summary-three", &[]),
        stop_script("answer", &[]),
    ]));
    let (handle, source) = fixture(root.path(), Arc::clone(&model), false).await;
    let mut events = handle.subscribe().expect("events");
    handle.send_message("new-input").await.expect("start");
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        }
    )));
    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    for index in 0..18 {
        let marker = format!("source-{index:02}");
        assert!(requests[..3].iter().any(|request| request.turns.iter().any(|turn|
            turn.blocks.iter().any(|block| matches!(block, rw_types::Block::Text{text} if text.contains(&marker))))));
    }
    assert!(
        requests[..3].iter().all(|request| request
            .turns
            .iter()
            .all(|turn| turn.blocks.iter().all(
                |block| !matches!(block, rw_types::Block::Text{text} if text == "new-input")
            )))
    );
    assert!(requests[3].turns.iter().any(|turn| {
        turn.blocks
            .iter()
            .any(|block| matches!(block, rw_types::Block::Text{text} if text == "new-input"))
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, PendingEvent::CompactionFinished { .. }))
            .count(),
        1
    );
    let captured = source.capture_history().await.expect("source");
    assert_eq!(captured.conversation().turns, 3);
    handle.close().await.expect("close");
}

#[tokio::test]
async fn failed_middle_page_preserves_full_canonical_generation() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(M3Model::new([
        stop_script("summary-one", &[]),
        vec![Err(ProviderError::new(
            ProviderErrorKind::Network,
            "summary unavailable",
        ))],
    ]));
    let (handle, source) = fixture(root.path(), Arc::clone(&model), false).await;
    let mut events = handle.subscribe().expect("events");
    handle.send_message("new-input").await.expect("start");
    let events = collect_turn(&mut events).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, PendingEvent::CompactionFailed { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, PendingEvent::CompactionFinished { .. }))
    );
    let captured = source.capture_history().await.expect("source");
    assert_eq!(captured.conversation().turns, 19);
    let page = captured
        .conversation_page(0..19, HistoryMaterializationLimits::default())
        .await
        .expect("unchanged source");
    assert!(
        page.turns[0]
            .blocks
            .iter()
            .any(|block| matches!(block, rw_types::Block::Text{text} if text == "source-00"))
    );
    handle.close().await.expect("close");
}

#[tokio::test]
async fn rolling_summary_never_reintroduces_evicted_or_pruned_payloads() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(M3Model::new([
        stop_script("summary-one", &[]),
        stop_script("summary-two", &[]),
        stop_script("summary-three", &[]),
        stop_script("answer", &[]),
    ]));
    let (handle, _) = fixture(root.path(), Arc::clone(&model), true).await;
    let mut events = handle.subscribe().expect("events");
    handle.send_message("new-input").await.expect("start");
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        }
    )));
    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        let request = serde_json::to_string(request).expect("fixture request");
        assert!(!request.contains("PRUNED_SECRET"));
        assert!(!request.contains("EVICTED_SECRET"));
    }
    assert!(requests[0].turns.iter().any(|turn| {
        turn.blocks.iter().any(|block|
        matches!(block, rw_types::Block::ToolResult { output: rw_types::ToolOutput::Text{text}, .. }
            if text == rw_context::PRUNED_TOOL_OUTPUT_REPLACEMENT))
    }));
    handle.close().await.expect("close");
}
