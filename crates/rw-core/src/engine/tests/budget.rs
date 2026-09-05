#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::tests::fixtures::models::M3Model;
use crate::engine::tests::fixtures::models::RoutedCostModel;
use crate::engine::tests::fixtures::sinks::AccountingRecordingSink;
use crate::engine::tests::fixtures::support::SessionEvent;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::stop_script;
use rw_providers::TokenUsage;
use rw_tools::ToolRegistry;
use rw_types::BudgetLevel;
use rw_types::BudgetScope;
use rw_types::BudgetUnit;
use rw_types::Cost;
use rw_types::Role;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

#[tokio::test]
async fn zero_budget_cap_stops_before_any_provider_or_compaction_call() {
    let root = TempDir::new().expect("tempdir");
    let mut model = M3Model::new([stop_script("must not run", &[])]);
    model.budget.session_cost_cap_micros_usd = Some(0);
    let model = Arc::new(model);
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("blocked")
        .await
        .expect("message accepted");
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::BudgetStatus {
            level: BudgetLevel::HardCap,
            ..
        }
    )));
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::BudgetExceeded,
            ..
        })
    ));
    assert!(model.requests().is_empty());
}

#[tokio::test]
async fn non_authoritative_sink_accumulates_session_cost_across_turns() {
    let root = TempDir::new().expect("tempdir");
    let billed_usage = TokenUsage {
        output_tokens: 600_000,
        ..TokenUsage::default()
    };
    let mut model = M3Model::new([
        stop_script("first billed response", &[billed_usage]),
        stop_script("second billed response", &[billed_usage]),
        stop_script("must remain unused", &[]),
    ]);
    model.budget.session_cost_cap_micros_usd = Some(1_000_000);
    let model = Arc::new(model);
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("first").await.expect("first message");
    let first = collect_turn(&mut events).await;
    assert!(matches!(
        first.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        })
    ));
    assert_eq!(model.requests().len(), 1);

    handle.send_message("second").await.expect("second message");
    let second = collect_turn(&mut events).await;
    assert_eq!(
        model.requests().len(),
        2,
        "the first turn must not be counted twice before the second dispatch"
    );
    assert!(second.iter().any(|event| matches!(
        event.kind,
        PendingEvent::BudgetStatus {
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Session,
            unit: BudgetUnit::MicrosUsd,
            current: 1_200_000,
            limit: 1_000_000,
            ..
        }
    )));
    assert!(matches!(
        second.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::BudgetExceeded,
            ..
        })
    ));

    handle.send_message("third").await.expect("third message");
    let third = collect_turn(&mut events).await;
    assert_eq!(
        model.requests().len(),
        2,
        "two completed $0.60 turns must block later dispatch under a $1.00 cap"
    );
    assert!(matches!(
        third.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::BudgetExceeded,
            ..
        })
    ));
}

#[tokio::test]
async fn authoritative_sink_does_not_double_count_local_session_cost() {
    let root = TempDir::new().expect("tempdir");
    let billed_usage = TokenUsage {
        output_tokens: 600_000,
        ..TokenUsage::default()
    };
    let mut model = M3Model::new([
        stop_script("first billed response", &[billed_usage]),
        stop_script("second billed response", &[billed_usage]),
    ]);
    model.budget.session_cost_cap_micros_usd = Some(1_000_000);
    let model = Arc::new(model);
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = Arc::new(AccountingRecordingSink::default());
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("first").await.expect("first message");
    collect_turn(&mut events).await;
    handle.send_message("second").await.expect("second message");
    let second = collect_turn(&mut events).await;

    assert_eq!(
        model.requests().len(),
        2,
        "authoritative ledger totals must replace, not add to, local history"
    );
    assert!(matches!(
        second.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::BudgetExceeded,
            ..
        })
    ));
}

#[tokio::test]
async fn daily_cap_fails_closed_without_an_authoritative_ledger() {
    let root = TempDir::new().expect("tempdir");
    let mut model = M3Model::new([stop_script("must remain unused", &[])]);
    model.budget.daily_cost_cap_micros_usd = Some(1_000_000);
    let model = Arc::new(model);
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("blocked").await.expect("message");
    let events = collect_turn(&mut events).await;

    assert!(model.requests().is_empty());
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::BudgetStatus {
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Daily,
            unit: BudgetUnit::MicrosUsd,
            current: 0,
            limit: 1_000_000,
            ..
        }
    )));
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::BudgetExceeded,
            ..
        })
    ));
}

#[tokio::test]
async fn incomplete_dollar_accounting_blocks_every_later_turn_before_provider_work() {
    for expected_scope in [BudgetScope::Session, BudgetScope::Daily] {
        let root = TempDir::new().expect("tempdir");
        let mut model = M3Model::new([
            stop_script("first billed response", &[]),
            stop_script("must remain unused", &[]),
        ]);
        model.cost_override = Some(Cost::Unavailable {
            reason: "fixture has no price".to_owned(),
        });
        match expected_scope {
            BudgetScope::Session => model.budget.session_cost_cap_micros_usd = Some(100),
            BudgetScope::Daily => model.budget.daily_cost_cap_micros_usd = Some(100),
            BudgetScope::TrailingMinute => unreachable!("fixture scope"),
        }
        let model = Arc::new(model);
        let sink = Arc::new(AccountingRecordingSink::default());
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink;
        let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
            .await
            .expect("actor");
        let mut events = handle.subscribe().expect("subscription");

        handle.send_message("first").await.expect("first message");
        collect_turn(&mut events).await;
        assert_eq!(model.requests().len(), 1);

        handle.send_message("second").await.expect("second message");
        let second = collect_turn(&mut events).await;
        assert_eq!(
            model.requests().len(),
            1,
            "an active dollar cap must fail closed after unpriced accounting"
        );
        assert!(second.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::BudgetStatus {
                level: BudgetLevel::HardCap,
                scope,
                unit: BudgetUnit::MicrosUsd,
                ..
            } if scope == &expected_scope
        )));
        assert!(matches!(
            second.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::BudgetExceeded,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn unavailable_credit_cost_preserves_response_and_blocks_later_dispatch() {
    for authoritative in [false, true] {
        let root = TempDir::new().expect("tempdir");
        let mut model = M3Model::new([
            stop_script("visible credit-billed response", &[]),
            stop_script("must remain unused", &[]),
        ]);
        model.cost_override = Some(Cost::Unavailable {
            reason: "credit burn unavailable".to_owned(),
        });
        model.budget.session_ai_credit_cap_micros = Some(100);
        let model = Arc::new(model);
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        if authoritative {
            actor_config.event_sink = Arc::new(AccountingRecordingSink::default());
        }
        let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
            .await
            .expect("actor");
        let mut events = handle.subscribe().expect("subscription");

        handle.send_message("first").await.expect("first message");
        let first = collect_turn(&mut events).await;
        assert_eq!(model.requests().len(), 1);
        assert!(first.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::TextDelta { text, .. }
                if text == "visible credit-billed response"
        )));
        assert!(first.iter().any(|event| matches!(
            event.kind,
            PendingEvent::BudgetStatus {
                level: BudgetLevel::HardCap,
                scope: BudgetScope::Session,
                unit: BudgetUnit::AiCreditMicros,
                current: 0,
                limit: 100,
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
        assert_eq!(
            model.requests().len(),
            1,
            "unknown credit burn must block later provider dispatch"
        );
        assert!(matches!(
            second.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::BudgetExceeded,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn opaque_route_cost_controls_post_response_hard_cap_with_shared_model_ids() {
    async fn run(route: &'static str) -> (Vec<SessionEvent>, usize) {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(RoutedCostModel::new(route));
        let handle = crate::engine::tests::fixtures::history::spawn(config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .await
        .expect("actor");
        let mut events = handle.subscribe().expect("subscription");
        handle.send_message("route me").await.expect("message");
        let events = collect_turn(&mut events).await;
        (events, model.requests.load(Ordering::SeqCst))
    }

    let (cheap, cheap_requests) = run("__model_cheap").await;
    assert_eq!(cheap_requests, 1);
    assert!(matches!(
        cheap.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            cost: Cost::Monetary {
                amount_micros: 10,
                ..
            },
            ..
        })
    ));
    assert!(cheap.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ConversationTurnCommitted {
            turn: Turn {
                role: Role::Assistant,
                meta: TurnMeta { model: Some(model), .. },
                ..
            },
            ..
        } if model == "cheap/shared-model-id"
    )));

    let (expensive, expensive_requests) = run("__model_expensive").await;
    assert_eq!(expensive_requests, 1);
    assert!(expensive.iter().any(|event| matches!(
        event.kind,
        PendingEvent::BudgetStatus {
            level: BudgetLevel::HardCap,
            current: 100,
            limit: 50,
            ..
        }
    )));
    assert!(matches!(
        expensive.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::BudgetExceeded,
            cost: Cost::Monetary {
                amount_micros: 100,
                ..
            },
            ..
        })
    ));
    assert!(expensive.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ConversationTurnCommitted {
            turn: Turn {
                role: Role::Assistant,
                meta: TurnMeta { model: Some(model), .. },
                ..
            },
            ..
        } if model == "expensive/shared-model-id"
    )));
}
