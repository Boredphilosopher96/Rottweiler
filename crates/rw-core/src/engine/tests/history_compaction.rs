#![cfg(test)]
use super::fixtures::{
    history,
    models::M3Model,
    support::{collect_turn, config, next_matching, stop_script, text_turn},
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
    async fn conversation_fragment(
        &self,
        cursor: crate::engine::recovery::ConversationFragmentCursor,
        max_bytes: usize,
    ) -> Result<HistoryRead<crate::engine::recovery::ConversationFragment>, AgentLoopError> {
        self.0.conversation_fragment(cursor, max_bytes).await
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

#[tokio::test]
async fn oversized_individual_block_is_summarized_with_complete_fragment_coverage() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(M3Model::new(
        (0..64).map(|_| stop_script("bounded summary", &[])),
    ));
    let original = rw_types::Block::ToolResult {
        id: rw_types::ToolCallId("source-call".into()),
        output: rw_types::ToolOutput::Structured {
            value: serde_json::json!({"payload": "🙂é\\\"\n".repeat(170_000), "end": "last-marker"}),
        },
        is_error: false,
    };
    let mut input = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    input.recovered.conversation = vec![rw_types::Turn {
        role: Role::Tool,
        blocks: vec![original.clone()],
        meta: rw_types::TurnMeta::default(),
    }];
    let input = history::bind(input).await.expect("source");
    let source = input.history.capture_history().await.expect("capture");
    let actor = SessionActor::spawn(input).expect("actor");
    let mut events = actor.subscribe().expect("events");
    actor.compact(None).await.expect("compact");
    next_matching(&mut events, |event| {
        matches!(event, PendingEvent::CompactionFinished { .. })
    })
    .await;
    let mut json = String::new();
    let mut fragments = 0;
    for request in model.requests() {
        for turn in request.turns {
            for block in turn.blocks {
                if let rw_types::Block::Text { text } = block
                    && text.starts_with("Canonical Tool tool_result block 0:0;")
                {
                    assert!(text.len() <= crate::engine::recovery::MAX_SUMMARY_FRAGMENT_BYTES);
                    json.push_str(text.split_once('\n').expect("frame").1);
                    fragments += 1;
                }
            }
        }
    }
    assert!(fragments > 1);
    assert_eq!(
        serde_json::from_str::<rw_types::Block>(&json).expect("complete source JSON"),
        original
    );
    let page = source
        .conversation_page(0..1, HistoryMaterializationLimits::default())
        .await
        .expect("immutable original");
    assert_eq!(page.turns[0].blocks[0], original);
    actor.close().await.expect("close");
}

#[tokio::test]
async fn oversized_pruned_block_never_reaches_summary_provider() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(M3Model::new([stop_script("bounded summary", &[])]));
    let mut input = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    input.recovered.conversation = vec![rw_types::Turn {
        role: Role::Tool,
        blocks: vec![rw_types::Block::ToolResult {
            id: rw_types::ToolCallId("source-call".into()),
            output: rw_types::ToolOutput::Text {
                text: "HIDDEN_FRAGMENT_PAYLOAD".repeat(100_000),
            },
            is_error: false,
        }],
        meta: rw_types::TurnMeta::default(),
    }];
    input
        .recovered
        .pruned_tool_outputs
        .insert("0:0".into(), 100_000);
    let actor = history::spawn(input).await.expect("actor");
    let mut events = actor.subscribe().expect("events");
    actor.compact(None).await.expect("compact");
    next_matching(&mut events, |event| {
        matches!(event, PendingEvent::CompactionFinished { .. })
    })
    .await;
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let text = serde_json::to_string(&requests).expect("requests");
    assert!(!text.contains("HIDDEN_FRAGMENT_PAYLOAD"));
    assert!(text.contains(rw_context::PRUNED_TOOL_OUTPUT_REPLACEMENT));
    actor.close().await.expect("close");
}

#[tokio::test]
async fn oversized_pinned_source_rejects_compaction_without_summary_dispatch() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(M3Model::new([stop_script("must not run", &[])]));
    let mut input = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    input.recovered.conversation = vec![text_turn(Role::User, "immutable pin ".repeat(100_000))];
    input
        .recovered
        .context_surgery
        .push(crate::engine::projection::ContextSurgeryAction {
            item_id: rw_types::context_source::conversation_item(SequenceId(0)),
            pinned: true,
            effective_after_agent_turn: 0,
        });
    let config = history::bind(input).await.expect("source");
    let history = config.history.capture_history().await.expect("capture");
    let actor = SessionActor::spawn(config).expect("actor");
    let error = actor.compact(None).await.expect_err("oversized pin");
    assert!(error.to_string().contains("pinned conversation"), "{error}");
    assert!(model.requests().is_empty());
    let page = history
        .conversation_page(0..1, HistoryMaterializationLimits::default())
        .await
        .expect("original pin");
    assert_eq!(
        page.turns[0],
        text_turn(Role::User, "immutable pin ".repeat(100_000))
    );
    actor.close().await.expect("close");
}

#[tokio::test]
async fn hook_expansion_is_checked_against_the_actual_summary_request_window() {
    use crate::engine::tests::fixtures::hooks::FixedHook;
    use rw_types::hook_contract::{HookClass, HookDirective, HookEvent, HookTransform};
    let root = tempfile::tempdir().expect("root");
    let mut model = M3Model::new([stop_script("must not run", &[])]);
    model.metadata = crate::engine::model::ModelContextMetadata {
        max_context_tokens: Some(2_000),
        max_output_tokens: Some(256),
        cache_breakpoints: None,
    };
    let model = Arc::new(model);
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            rw_ext::HookRegistration::new(
                "summary.expand",
                HookEvent::PreCompact,
                HookClass::Transform,
            ),
            FixedHook {
                label: "expand",
                calls: Arc::new(std::sync::Mutex::new(Vec::new())),
                result: Ok(HookDirective::Transform {
                    change: HookTransform::PreCompact {
                        injected_context: vec!["hook context ".repeat(2_000)],
                        replacement_prompt: None,
                        suppress_auto_continue: false,
                    },
                }),
            },
        )
        .expect("hook");
    let mut input = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        hooks,
    );
    input.recovered.conversation = vec![text_turn(Role::User, "small input")];
    let actor = history::spawn(input).await.expect("actor");
    let error = actor.compact(None).await.expect_err("expanded request");
    assert!(
        error.to_string().contains("including hook output"),
        "{error}"
    );
    assert!(model.requests().is_empty());
    actor.close().await.expect("close");
}
