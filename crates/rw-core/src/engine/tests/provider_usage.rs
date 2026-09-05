#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::SessionUsage;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::model::ModelContextMetadata;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::M3Model;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::AccountingRecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use rw_providers::CacheBreakpointSupport;
use rw_providers::ProviderError;
use rw_providers::ProviderErrorKind;
use rw_providers::ProviderEvent;
use rw_providers::TokenUsage;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::Block;
use rw_types::BudgetLevel;
use rw_types::BudgetScope;
use rw_types::BudgetUnit;
use rw_types::Cost;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn provider_error_preserves_partial_output_and_emits_failed_terminal() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([vec![
        Ok(ProviderEvent::MessageStart {
            model: "fixture-model".to_owned(),
        }),
        Ok(ProviderEvent::TextDelta {
            text: "partial".to_owned(),
        }),
        Err(ProviderError::new(
            ProviderErrorKind::Network,
            "fixture stream failed",
        )),
    ]]));
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::TextDelta { text, .. } if text == "partial"
    )));
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::Failed,
            ..
        })
    ));
    let context = handle.dump_prompt(None).await.expect("partial context");
    assert!(context.turns.iter().any(|turn| {
        turn.blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text == "partial"))
    }));
}

#[tokio::test]
async fn usage_accumulates_latest_totals_once_per_provider_iteration() {
    let root = TempDir::new().expect("tempdir");
    let first_latest = TokenUsage {
        input_tokens: 7,
        output_tokens: 3,
        cache_read_tokens: 2,
        cache_write_tokens: 1,
        reasoning_tokens: 4,
    };
    let second = TokenUsage {
        input_tokens: 11,
        output_tokens: 5,
        cache_read_tokens: 3,
        cache_write_tokens: 2,
        reasoning_tokens: 6,
    };
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[("call", "fixture", json!({}))],
            &[
                TokenUsage {
                    input_tokens: 2,
                    ..TokenUsage::default()
                },
                first_latest,
            ],
        ),
        stop_script("done", &[second]),
    ]));
    let tool = Arc::new(StubTool::new(
        "fixture",
        vec![],
        StubOutcome::Success(ToolResult::new("ok", Value::Null)),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(tool).expect("register tool");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let finished = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { .. })
    })
    .await;
    assert!(matches!(
        finished.kind,
        PendingEvent::TurnFinished {
            usage: SessionUsage {
                input_tokens: 18,
                output_tokens: 8,
                cache_read_tokens: 5,
                cache_write_tokens: 3,
                reasoning_tokens: 10,
            },
            ..
        }
    ));
}

#[test]
fn usage_counters_round_trip_as_js_safe_decimal_strings() {
    let usage = SessionUsage {
        input_tokens: u64::MAX,
        output_tokens: u64::MAX - 1,
        cache_read_tokens: u64::MAX - 2,
        cache_write_tokens: u64::MAX - 3,
        reasoning_tokens: u64::MAX - 4,
    };
    let encoded = serde_json::to_string(&usage).expect("serialize usage");
    assert!(encoded.contains("\"input_tokens\":\"18446744073709551615\""));
    let decoded: SessionUsage = serde_json::from_str(&encoded).expect("deserialize usage");
    assert_eq!(decoded, usage);
}

#[tokio::test]
async fn provider_usage_reconciles_next_meter_and_surfaces_cache_hits() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(M3Model::new([
        tool_script(
            &[("call-1", "ok", json!({}))],
            &[TokenUsage {
                input_tokens: 500,
                cache_read_tokens: 500,
                ..TokenUsage::default()
            }],
        ),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "ok",
            vec![],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        )))
        .expect("register tool");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::ContextUsage {
            cache_hit_basis_points: 5_000,
            provider_input_tokens: 1_000,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::ContextUsage {
            provider_input_tokens: 0,
            correction_millionths: 4_000_000,
            ..
        }
    )));
}

#[tokio::test]
async fn known_subscription_capacity_produces_nonzero_post_turn_context_usage() {
    let root = TempDir::new().expect("tempdir");
    let mut model = M3Model::new([stop_script(
        "subscription response",
        &[TokenUsage {
            input_tokens: 700,
            output_tokens: 36,
            ..TokenUsage::default()
        }],
    )]);
    // This is the exact capability shape produced by the enriched
    // openai_codex route for a known 400k model.
    model.metadata = ModelContextMetadata {
        max_context_tokens: Some(400_000),
        max_output_tokens: Some(128_000),
        cache_breakpoints: Some(CacheBreakpointSupport::Automatic),
    };
    model.cost_override = Some(Cost::SubscriptionQuota {
        used: Some("736".to_owned()),
        unit: Some("tokens".to_owned()),
    });
    let handle = SessionActor::spawn(config(
        root.path(),
        Arc::new(model),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("complete a turn")
        .await
        .expect("message");
    let events = collect_turn(&mut events).await;

    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::ContextUsage {
            usable_tokens: 380_000,
            reserved_tokens: 20_000,
            context_window_known: true,
            provider_input_tokens: 700,
            ..
        }
    )));
}

#[tokio::test]
async fn subscription_token_cap_stops_after_the_response_and_blocks_later_dispatch() {
    let root = TempDir::new().expect("tempdir");
    let mut model = M3Model::new([
        stop_script("visible response", &[]),
        stop_script("must remain unused", &[]),
    ]);
    model.cost_override = Some(Cost::SubscriptionQuota {
        used: Some("736".to_owned()),
        unit: Some("tokens".to_owned()),
    });
    model.budget.session_token_cap = Some(700);
    let model = Arc::new(model);
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = Arc::new(AccountingRecordingSink::default());
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("first").await.expect("first message");
    let first = collect_turn(&mut events).await;
    assert!(first.iter().any(|event| matches!(
        event.kind,
        PendingEvent::BudgetStatus {
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Session,
            unit: BudgetUnit::Tokens,
            current: 736,
            limit: 700,
            ..
        }
    )));
    assert!(matches!(
        first.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::BudgetExceeded,
            ..
        })
    ));

    handle.send_message("second").await.expect("second message");
    let second = collect_turn(&mut events).await;
    assert_eq!(model.requests().len(), 1);
    assert!(matches!(
        second.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::BudgetExceeded,
            ..
        })
    ));
}
