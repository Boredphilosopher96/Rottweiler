#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::MessageDisposition;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::durability::NoopSessionEventSink;
use crate::engine::model::ModelContextMetadata;
use crate::engine::model::ModelDriver;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::project_session_events;
use crate::engine::session::SessionActor;
use crate::engine::session::SessionHandle;
use crate::engine::tests::fixtures::models::DelayedSummaryModel;
use crate::engine::tests::fixtures::models::GatedCompactionModel;
use crate::engine::tests::fixtures::models::M3Model;
use crate::engine::tests::fixtures::models::ReplayHarnessModel;
use crate::engine::tests::fixtures::models::ReplaySourceProvider;
use crate::engine::tests::fixtures::sinks::AccountingRecordingSink;
use crate::engine::tests::fixtures::sinks::FailCompactionLedgerSink;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::SessionEvent;
use crate::engine::tests::fixtures::support::TestEventSinkExt;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::collect_wire_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::text_turn;
use rw_providers::CacheBreakpointSupport;
use rw_providers::FixtureRedactor;
use rw_providers::ProviderError;
use rw_providers::ProviderErrorKind;
use rw_providers::ProviderEvent;
use rw_providers::Recorder;
use rw_providers::ReplayProvider;
use rw_providers::TokenUsage;
use rw_tools::ToolRegistry;
use rw_types::AccountingAttribution;
use rw_types::Block;
use rw_types::CompactionReason;
use rw_types::EngineEvent;
use rw_types::Role;
use rw_types::config::PermissionDecision;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::time::timeout;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn one_hundred_fifty_turn_overflow_compacts_and_continues_through_actor() {
    let root = TempDir::new().expect("tempdir");
    let mut compaction_script = stop_script(
        "## Goal\ncontinue\n\n## Instructions\nkeep intent\n\n## Discoveries\nsrc/lib.rs checksum amber-42\n\n## Accomplished\n150 turns\n\n## Relevant files & directories\nsrc/lib.rs\nPROJECT.md",
        &[TokenUsage {
            input_tokens: 2_000,
            output_tokens: 60,
            ..TokenUsage::default()
        }],
    );
    compaction_script.insert(
        0,
        Ok(ProviderEvent::ThinkingDelta {
            content: "Identifying durable context".to_owned(),
            signature: None,
        }),
    );
    let mut model = M3Model::new([compaction_script, stop_script("amber-42", &[])]);
    model.metadata = ModelContextMetadata {
        max_context_tokens: Some(2_000),
        max_output_tokens: Some(256),
        cache_breakpoints: Some(CacheBreakpointSupport::Explicit),
    };
    model.budget.session_cost_cap_micros_usd = Some(100);
    let model = Arc::new(model);
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.conversation = (0..150)
        .map(|index| {
            text_turn(
                if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                if index == 0 {
                    "src/lib.rs checksum amber-42".to_owned()
                } else {
                    format!("turn {index}: {}", "context ".repeat(20))
                },
            )
        })
        .collect();
    let sink = Arc::new(NoopSessionEventSink::default());
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    let mut wire_events = handle.subscribe().expect("subscription");
    handle
        .send_message("What is the src/lib.rs checksum?")
        .await
        .expect("message");
    let events = collect_turn(&mut events).await;
    let wire_events = collect_wire_turn(&mut wire_events).await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::CompactionStarted {
            reason: CompactionReason::Automatic
        }
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, PendingEvent::CompactionFinished { .. }))
    );
    assert!(wire_events.iter().any(|event| matches!(
        event,
        EngineEvent::CompactionAttemptStarted { attempt: 0, .. }
    )));
    assert!(wire_events.iter().any(|event| matches!(
        event,
        EngineEvent::CompactionThinkingDelta { attempt: 0, text, .. }
            if text == "Identifying durable context"
    )));
    assert!(wire_events.iter().any(|event| matches!(
        event,
        EngineEvent::CompactionTextDelta { attempt: 0, text, .. }
            if text.contains("src/lib.rs checksum amber-42")
    )));
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].tools.is_empty());
    assert!(requests[1].turns.iter().any(|turn| {
            turn.role == Role::User
                && matches!(turn.blocks.as_slice(), [Block::Text { text }] if text == rw_context::AUTO_CONTINUE_TEXT)
        }));
    let final_prompt = serde_json::to_string(&requests[1].turns).expect("serialize prompt");
    assert!(final_prompt.contains("amber-42"));
    assert!(!final_prompt.contains("turn 149:"));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::TextDelta { text, .. } if text == "amber-42"
    )));
    assert!(requests[1].cache_hint.is_none());
    let durable = sink.test_events_after(None).await.expect("durable events");
    let resumed = project_session_events(&durable).expect("resume projection");
    assert!(
        resumed
            .conversation
            .first()
            .is_some_and(|turn| turn.meta.summary)
    );
    assert!(resumed.conversation.len() < 10);
}

#[tokio::test]
async fn post_summary_compaction_failure_emits_correlated_terminal() {
    let root = TempDir::new().expect("tempdir");
    let mut model = M3Model::new([stop_script("durable compacted summary", &[])]);
    model.metadata = ModelContextMetadata {
        max_context_tokens: Some(600),
        max_output_tokens: Some(128),
        cache_breakpoints: None,
    };
    let mut actor_config = config(
        root.path(),
        Arc::new(model),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.conversation = (0..40)
        .map(|index| {
            text_turn(
                if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                format!("turn {index}: {}", "context ".repeat(20)),
            )
        })
        .collect();
    let sink = Arc::new(FailCompactionLedgerSink::default());
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("continue").await.expect("message");
    let events = collect_turn(&mut events).await;

    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::CompactionFailed { summary_turn: 1 }
    )));
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Failed,
            ..
        }
    )));
    let durable = sink.test_events_after(None).await.expect("durable events");
    assert!(durable.iter().any(|event| matches!(
        event,
        EngineEvent::CompactionFailed {
            summary_turn_id,
            ..
        } if summary_turn_id.0 == "1"
    )));
    assert!(
        !durable
            .iter()
            .any(|event| matches!(event, EngineEvent::CompactionFinished { .. }))
    );
}

#[tokio::test]
async fn one_hundred_fifty_turn_compaction_quality_replays_from_recorded_provider_fixtures() {
    async fn run(root: &Path, model: Arc<dyn ModelDriver>) -> (Vec<SessionEvent>, SessionHandle) {
        let mut actor_config = config(
            root,
            model,
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.recovered.conversation = (0..150)
            .map(|index| {
                text_turn(
                    if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    if index == 0 {
                        "src/lib.rs checksum amber-42".to_owned()
                    } else {
                        format!("turn {index}: {}", "context ".repeat(20))
                    },
                )
            })
            .collect();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe().expect("subscription");
        handle
            .send_message("What is the src/lib.rs checksum?")
            .await
            .expect("message");
        (collect_turn(&mut events).await, handle)
    }

    let fixture_directory = TempDir::new().expect("fixture directory");
    let source = Arc::new(ReplaySourceProvider {
            scripts: Mutex::new(
                [
                    stop_script(
                        "## Goal\ncontinue\n\n## Instructions\nkeep intent\n\n## Discoveries\nsrc/lib.rs checksum amber-42\n\n## Accomplished\n150 turns\n\n## Relevant files & directories\nsrc/lib.rs\nPROJECT.md",
                        &[TokenUsage {
                            input_tokens: 2_000,
                            output_tokens: 60,
                            ..TokenUsage::default()
                        }],
                    ),
                    stop_script("amber-42", &[]),
                ]
                .into_iter()
                .collect(),
            ),
        });
    let recorder = Arc::new(Recorder::new(
        source,
        fixture_directory.path(),
        FixtureRedactor::default(),
    ));
    let recording_root = TempDir::new().expect("recording workspace");
    let (_recorded_events, recorded_handle) = run(
        recording_root.path(),
        Arc::new(ReplayHarnessModel::new(recorder.clone())),
    )
    .await;
    drop(recorded_handle);
    recorder.flush().await.expect("provider fixtures flush");

    let replay = Arc::new(
        ReplayProvider::load("context-replay", fixture_directory.path())
            .await
            .expect("replay fixtures load"),
    );
    let replay_root = TempDir::new().expect("replay workspace");
    let (events, handle) = run(
        replay_root.path(),
        Arc::new(ReplayHarnessModel::new(replay)),
    )
    .await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::CompactionStarted {
            reason: CompactionReason::Automatic
        }
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::TextDelta { text, .. } if text == "amber-42"
    )));
    let dump = handle
        .dump_prompt(None)
        .await
        .expect("post-replay prompt dump");
    let prompt = serde_json::to_string(&dump.turns).expect("serialize replay prompt");
    assert!(prompt.contains("amber-42"));
    assert!(!prompt.contains("turn 149:"));
}

#[tokio::test]
async fn typed_provider_overflow_compacts_then_replays_last_real_user() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(M3Model::new([
        vec![Err(ProviderError::new(
            ProviderErrorKind::ContextOverflow,
            "sanitized overflow",
        ))],
        stop_script(
            "## Goal\nrecover\n\n## Instructions\n\n## Discoveries\n\n## Accomplished\n\n## Relevant files & directories\n",
            &[],
        ),
        stop_script("recovered answer", &[]),
    ]));
    let handle = SessionActor::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("keep me intact")
        .await
        .expect("message");
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::CompactionStarted {
            reason: CompactionReason::ProviderOverflow
        }
    )));
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].turns.iter().any(|turn| {
        !turn.meta.synthetic
            && turn.role == Role::User
            && matches!(turn.blocks.as_slice(), [Block::Text { text }] if text == "keep me intact")
    }));
    assert!(!requests[2].turns.iter().any(|turn| {
            matches!(turn.blocks.as_slice(), [Block::Text { text }] if text == rw_context::AUTO_CONTINUE_TEXT)
        }));
}

#[tokio::test]
async fn manual_compaction_keeps_queries_and_interrupt_responsive() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(DelayedSummaryModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    actor_config.recovered.conversation = vec![text_turn(Role::User, "compact me")];
    let handle = SessionActor::spawn(actor_config).expect("actor");
    handle.ensure_local_driver().await.expect("driver");
    let compact_handle = handle.clone();
    let compact = tokio::spawn(async move { compact_handle.compact(None).await });
    timeout(Duration::from_millis(100), async {
        loop {
            if handle.active_turn.load(Ordering::Acquire) != 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("manual compaction must start");
    timeout(Duration::from_millis(100), handle.context_snapshot())
        .await
        .expect("query must remain responsive")
        .expect("context query");
    assert!(handle.interrupt().await.expect("interrupt"));
    let result = timeout(Duration::from_secs(1), compact)
        .await
        .expect("compaction cancellation timeout")
        .expect("compaction task join");
    assert!(result.is_err());
    let cost = handle
        .cost_snapshot()
        .await
        .expect("cancelled compaction cost");
    let cancelled = cost
        .turns
        .iter()
        .filter(|entry| entry.attribution == AccountingAttribution::Compaction)
        .collect::<Vec<_>>();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].usage.input_tokens, 11);
    assert_eq!(cancelled[0].usage.output_tokens, 7);
    let durable = sink
        .test_events_after(None)
        .await
        .expect("durable cancellation events");
    let first = project_session_events(&durable).expect("first cancellation resume");
    let second = project_session_events(&durable).expect("second cancellation resume");
    assert_eq!(first.accounting, second.accounting);
    assert_eq!(first.accounting.len(), 1);
    assert!(!first.interrupted_compaction);
}

#[tokio::test]
async fn messages_queued_during_manual_compaction_resume_in_fifo_order() {
    let root = TempDir::new().expect("tempdir");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let model = Arc::new(GatedCompactionModel {
        calls: AtomicUsize::new(0),
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.conversation = vec![text_turn(Role::User, "compact me")];
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    let compact_handle = handle.clone();
    let compact = tokio::spawn(async move { compact_handle.compact(None).await });
    timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("compaction provider started");

    assert_eq!(
        handle
            .send_message("queued first")
            .await
            .expect("first queue"),
        MessageDisposition::Queued
    );
    assert_eq!(
        handle
            .send_message("queued second")
            .await
            .expect("second queue"),
        MessageDisposition::Queued
    );
    release.notify_one();
    compact
        .await
        .expect("compaction join")
        .expect("compaction completion");
    collect_turn(&mut events).await;

    let snapshot = handle.snapshot().await.expect("conversation snapshot");
    let queued = snapshot
        .conversation
        .iter()
        .filter_map(|turn| {
            if turn.role != Role::User {
                return None;
            }
            match turn.blocks.as_slice() {
                [Block::Text { text }] => Some(text.as_str()),
                _ => None,
            }
        })
        .filter(|text| text.starts_with("queued "))
        .collect::<Vec<_>>();
    assert_eq!(queued, ["queued first", "queued second"]);
}

#[tokio::test]
async fn failed_compaction_alias_usage_is_accounted_before_successful_fallback() {
    let root = TempDir::new().expect("tempdir");
    let first_attempt = vec![
        Ok(ProviderEvent::MessageStart {
            model: "failed-compaction-model".to_owned(),
        }),
        Ok(ProviderEvent::Usage {
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 60,
                ..TokenUsage::default()
            },
        }),
        Err(ProviderError::new(
            ProviderErrorKind::Network,
            "sanitized failed compaction alias",
        )),
    ];
    let mut model = M3Model::new([
        first_attempt,
        stop_script(
            "## Goal\ncontinue\n\n## Instructions\n\n## Discoveries\nfallback worked\n\n## Accomplished\n\n## Relevant files & directories\n",
            &[TokenUsage {
                input_tokens: 80,
                output_tokens: 20,
                ..TokenUsage::default()
            }],
        ),
    ]);
    model.compaction.model_alias = Some("compact-first".to_owned());
    model.budget.session_cost_cap_micros_usd = Some(100);
    let model = Arc::new(model);
    let sink = Arc::new(AccountingRecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    actor_config.recovered.conversation = vec![text_turn(Role::User, "retain this")];
    let handle = SessionActor::spawn(actor_config).expect("actor");
    handle.compact(None).await.expect("fallback compaction");

    let snapshot = handle
        .cost_snapshot()
        .await
        .expect("compaction cost snapshot");
    let compaction = snapshot
        .turns
        .iter()
        .filter(|entry| entry.attribution == AccountingAttribution::Compaction)
        .collect::<Vec<_>>();
    assert_eq!(compaction.len(), 2);
    assert_eq!(compaction[0].usage.output_tokens, 60);
    assert_eq!(compaction[1].usage.output_tokens, 20);
    assert_eq!(snapshot.session_cost_micros_usd, 80);

    let durable = sink
        .test_events_after(None)
        .await
        .expect("durable fallback events");
    assert_eq!(
        durable
            .iter()
            .filter(|event| matches!(event, EngineEvent::CompactionAttemptFinished { .. }))
            .count(),
        1
    );
    assert_eq!(
        durable
            .iter()
            .filter(|event| matches!(event, EngineEvent::CompactionFinished { .. }))
            .count(),
        1
    );
    let resumed = project_session_events(&durable).expect("fallback resume");
    assert_eq!(resumed.accounting.len(), 2);
}
