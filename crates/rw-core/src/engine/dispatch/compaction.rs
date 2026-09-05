use crate::engine::AgentLoopError;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::PreparedModelSwitch;
use crate::engine::session::ProtocolCompletion;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::BudgetUsage;
use crate::engine::turn::RunningTurn;
use crate::engine::turn::TurnSignal;
use crate::engine::turn::compact_during_turn;
use crate::engine::turn::evaluate_budget;
use crate::engine::turn::persist_event;
use crate::engine::turn::session_accounting_fallback;
use rw_tools::CancellationToken;
use rw_types::CompactionReason;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub(super) fn start_manual_compaction(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    turn_signals: &mpsc::UnboundedSender<TurnSignal>,
    active_turn: &Arc<AtomicU64>,
    instructions: Option<String>,
    model_switch: Option<PreparedModelSwitch>,
    completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
) {
    let summary_turn = state.next_turn;
    let cancellation = CancellationToken::default();
    state.running = Some(RunningTurn {
        id: summary_turn,
        cancellation: cancellation.clone(),
        caused_by: state.transient_cause.clone(),
    });
    state.control.start(summary_turn, cancellation.clone());
    active_turn.store(summary_turn, Ordering::Release);
    let mut conversation = state.conversation.clone();
    let mut context_surgery = state.context_surgery.clone();
    let local_session_accounting = session_accounting_fallback(&state.accounting);
    let config = Arc::new(config.with_model_route_and_mode(
        state.model_alias.clone(),
        state.provider.clone(),
        &state.mode_id,
    ));
    let signals = turn_signals.clone();
    let tasks = state.tasks.clone();
    if let Err(error) = tasks.spawn(Arc::clone(&config), cancellation.clone(), async move {
        let result = async {
            let pre_budget = evaluate_budget(
                summary_turn,
                config.event_clock.as_ref(),
                &config.event_sink,
                &config.model.budget_config(),
                local_session_accounting,
                BudgetUsage::default(),
            )
            .await?;
            for event in pre_budget.events {
                persist_event(&signals, event).await?;
            }
            if pre_budget.hard_stop {
                return Err(AgentLoopError::InvalidConfiguration(
                    "budget hard cap prevents compaction model call".to_owned(),
                ));
            }
            compact_during_turn(
                summary_turn,
                &mut conversation,
                &mut context_surgery,
                CompactionReason::Manual,
                &config,
                &cancellation,
                &signals,
                local_session_accounting,
                0,
                0,
                0,
                instructions,
            )
            .await
            .map(|_| ())
        }
        .await;
        if let Err(error) = &result {
            let _ = persist_event(
                &signals,
                PendingEvent::Error {
                    message: error.to_string(),
                },
            )
            .await;
        }
        let _ = signals.send(TurnSignal::ManualCompactionComplete {
            turn: summary_turn,
            conversation,
            context_surgery,
            result,
            model_switch,
            completion,
        });
    }) {
        state.unsettled = Some(error.to_string());
        state.tasks.cancel();
    }
}
