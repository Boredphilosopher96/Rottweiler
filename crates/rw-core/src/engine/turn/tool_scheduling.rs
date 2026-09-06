use crate::PermissionApprover;
use crate::engine::MAX_TOOL_EXECUTION_WINDOW;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::SessionActorConfig;
use crate::engine::task_ownership;
use crate::engine::turn::hooks::mark_unsettled;
use crate::engine::turn::ordered_output::OrderedOutputCoordinator;
use crate::engine::turn::provider_messages::emit_plan_submission;
use crate::engine::turn::provider_messages::persist_event;
use crate::engine::turn::redaction::redact_tool_output;
use crate::engine::turn::signals::TurnSignal;
use crate::engine::turn::subagent_events::OrderedSubagentCoordinator;
use crate::engine::turn::tool_execution::ToolExecutionRuntime;
use crate::engine::turn::tool_execution::execute_prepared_tool;
use crate::engine::turn::tool_requests::PreparedToolCall;
use crate::engine::turn::tool_requests::failed_execution;
use crate::engine::turn::tool_requests::prepare_tool_call;
use futures_util::StreamExt;
use rw_tools::CancellationToken;
use rw_tools::SubagentLifecycleMode;
use rw_tools::ToolContext;
use rw_types::SessionMode;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) use super::doom_loop::DoomLoopGuard;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[tracing::instrument(target = "rw_performance", level = "trace", name = "tool.batch", skip_all, fields(session_id = config.session_id.0.as_str(), turn, calls = calls.calls.len()))]
pub(super) async fn execute_tool_calls(
    turn: u64,
    tasks: &task_ownership::ActorTasks,
    calls: super::tool_admission::AdmittedToolBatch,
    config: &Arc<SessionActorConfig>,
    context: &ToolContext,
    cancellation: &CancellationToken,
    approver: &dyn PermissionApprover,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    mode: SessionMode,
) -> Result<super::tool_requests::CommittedToolBatch, crate::engine::AgentLoopError> {
    let mut failure = None;
    let retained =
        super::tool_result_budget::ToolResultBudget::new(config, calls.calls.len()).await?;
    let super::tool_admission::AdmittedToolBatch { calls, mut budget } = calls;
    let mut profiles =
        super::tool_result_closure::ResultProfiles::new(calls.iter().map(|(call, _)| call))?;
    let mut prepared = Vec::with_capacity(calls.len());
    for (call, displayed) in calls {
        let mut preparation = prepare_tool_call(
            turn,
            call,
            config,
            approver,
            cancellation,
            signals,
            context,
            mode,
            &mut budget,
            displayed,
        )
        .await;
        if let PreparedToolCall::Complete(execution) = &mut preparation {
            retained.admit_execution(execution);
        }
        prepared.push(preparation);
    }
    let coordinator = Arc::new(OrderedOutputCoordinator::new(
        turn,
        signals.clone(),
        Arc::clone(&config.secret_redactor),
    ));
    let subagent_indices = prepared.iter().filter_map(|call| {
        let PreparedToolCall::Execute { call, .. } = call else {
            return None;
        };
        match config.tools.subagent_lifecycle_mode(&call.name) {
            Some(SubagentLifecycleMode::Single) => Some((call.index, false)),
            Some(SubagentLifecycleMode::MultipleOrdered) => Some((call.index, true)),
            Some(SubagentLifecycleMode::None) | None => None,
        }
    });
    let subagents = Arc::new(OrderedSubagentCoordinator::new_with_multi(
        subagent_indices,
        signals.clone(),
    ));
    let execution_runtime = ToolExecutionRuntime {
        result_budget: retained.clone(),
        coordinator: Arc::clone(&coordinator),
        checkpoints: Arc::clone(&config.checkpoints),
        hooks: Arc::clone(&config.hooks),
        secret_redactor: Arc::clone(&config.secret_redactor),
        signals: signals.clone(),
        turn,
        subagents: Arc::clone(&subagents),
        tools: Arc::clone(&config.tools),
        session_id: config.session_id.clone(),
    };
    let total = prepared.len();
    let mut ordered = Vec::with_capacity(total);
    let mut prepared = prepared.into_iter().peekable();
    let mut running = futures_util::stream::FuturesUnordered::new();
    let mut completed = BTreeMap::new();
    let mut next = 0;
    let mut launched = 0;
    let mut mutation_running = false;
    while next < total {
        // Limit the whole ordered window, including completed later results.
        // Refilling solely by active task count would retain an unbounded tail
        // while the first call waits or produces output.
        while !mutation_running && launched - next < MAX_TOOL_EXECUTION_WINDOW {
            let Some(front) = prepared.peek() else {
                break;
            };
            let mutation = matches!(
                front,
                PreparedToolCall::Execute {
                    read_only: false,
                    ..
                }
            );
            if mutation && launched != next {
                break;
            }
            let Some(call) = prepared.next() else {
                break;
            };
            let index = launched;
            launched += 1;
            match call {
                PreparedToolCall::Complete(execution) => {
                    completed.insert(index, (execution, false));
                }
                PreparedToolCall::Execute { call, .. } if cancellation.is_cancelled() => {
                    completed.insert(
                        index,
                        (
                            failed_execution(call, "tool execution cancelled before start"),
                            false,
                        ),
                    );
                }
                call @ PreparedToolCall::Execute { .. } => {
                    let fallback = call.call().clone();
                    let context = context.clone();
                    let cancellation = cancellation.clone();
                    let runtime = execution_runtime.clone();
                    let task = tasks.spawn(Arc::clone(config), cancellation.clone(), async move {
                        execute_prepared_tool(call, context, cancellation, runtime).await
                    });
                    running.push(async move {
                        let execution = async {
                            let task = task.ok()?;
                            task.await.ok().map(|(execution, _ran)| execution)
                        }
                        .await
                        .unwrap_or_else(|| {
                            let mut execution =
                                failed_execution(fallback, "tool task ended without a result");
                            execution.unsettled = true;
                            execution
                        });
                        (index, execution, mutation)
                    });
                    mutation_running = mutation;
                }
            }
        }
        let Some((mut execution, was_mutation)) = completed.remove(&next) else {
            if let Some((index, execution, mutation)) = running.next().await {
                completed.insert(index, (execution, mutation));
            }
            continue;
        };
        if was_mutation {
            mutation_running = false;
        }
        if execution.unsettled {
            mark_unsettled(
                signals,
                cancellation,
                "tool invocation effects remain unproven".to_owned(),
            );
            let execution_index = execution.call.index;
            failure.get_or_insert_with(|| {
                crate::engine::AgentLoopError::EffectsUnsettled(
                    "tool invocation effects remain unproven".into(),
                )
            });
            next = next.saturating_add(1);
            coordinator.advance(next);
            subagents.advance_after_tool(execution_index);
            continue;
        }
        retained.admit_execution(&mut execution);
        redact_tool_output(&mut execution.output, config.secret_redactor.as_ref());
        retained.admit_execution(&mut execution);
        let admitted = profiles
            .admit(next, execution, turn, tasks, config, &retained, signals)
            .await;
        let execution = match admitted {
            Ok(execution) => execution,
            Err(error) => {
                mark_unsettled(signals, cancellation, error.to_string());
                failure.get_or_insert(error);
                next = next.saturating_add(1);
                coordinator.advance(next);
                subagents.advance_after_tool(next - 1);
                continue;
            }
        };
        retained.settled(&execution);
        let presentation = execution.presentation.as_ref().and_then(|plan| {
            plan.project(&execution.output, |text| {
                config.secret_redactor.redact(text)
            })
            .map_err(
                |error| tracing::warn!(reason = %error, "tool presentation could not be produced"),
            )
            .ok()
        });
        emit_plan_submission(
            &execution,
            mode,
            signals,
            config.secret_redactor.as_ref(),
            &config.tools,
        );
        let committed = persist_event(
            signals,
            PendingEvent::ToolCallFinished {
                presentation,
                turn,
                id: execution.call.id.clone(),
                invocation_id: execution.call.invocation_id.clone(),
                output: execution.output.clone(),
                is_error: execution.is_error,
                index: execution.call.index,
            },
        )
        .await;
        let execution_index = execution.call.index;
        match committed {
            Ok(meta) => ordered.push(super::tool_requests::CommittedToolExecution {
                execution,
                source: meta.sequence_id,
            }),
            Err(error) => {
                cancellation.cancel();
                failure.get_or_insert(error);
            }
        }
        next = next.saturating_add(1);
        coordinator.advance(next);
        subagents.advance_after_tool(execution_index);
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(super::tool_requests::CommittedToolBatch {
            executions: ordered,
            retained,
        }),
    }
}
