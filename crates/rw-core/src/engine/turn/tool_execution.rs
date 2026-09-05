use crate::PermissionGate;
use crate::PermissionRequest;
use crate::engine::AgentLoopError;
use crate::engine::TOOL_CANCELLATION_GRACE;
use crate::engine::approval_diff;
use crate::engine::diff_binding;
use crate::engine::mutation_checkpoints::MutationCheckpointCoordinator;
use crate::engine::mutation_checkpoints::MutationCheckpointOutcome;
use crate::engine::redaction::SecretRedactor;
use crate::engine::turn::hooks::dispatch_hook;
use crate::engine::turn::hooks::dispatch_tool_hook_effect;
use crate::engine::turn::hooks::hook_rejection;
use crate::engine::turn::hooks::mark_unsettled;
use crate::engine::turn::hooks::report_hook_failures;
use crate::engine::turn::ordered_output::OrderedOutputCoordinator;
use crate::engine::turn::ordered_output::OrderedOutputSink;
use crate::engine::turn::progress::InvocationProgress;
use crate::engine::turn::redaction::redact_tool_output;
use crate::engine::turn::redaction::redacted_json;
use crate::engine::turn::signals::TurnSignal;
use crate::engine::turn::subagent_events::ActorSubagentEventSink;
use crate::engine::turn::subagent_events::ActorSubagentLifecycleState;
use crate::engine::turn::subagent_events::OrderedSubagentCoordinator;
use crate::engine::turn::tool_requests::PendingToolCall;
use crate::engine::turn::tool_requests::PreparedToolCall;
use crate::engine::turn::tool_requests::ToolExecution;
use crate::engine::turn::tool_requests::background_control_call;
use crate::engine::turn::tool_requests::failed_execution;
use futures_util::FutureExt;
use rw_ext::HookDispatcher;
use rw_ext::HookEffect;
use rw_ext::HookEvent;
use rw_tools::CancellationToken;
use rw_tools::MutationScope;
use rw_tools::SubagentEventSink;
use rw_tools::ToolContext;
use rw_tools::ToolError;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::SessionId;
use rw_types::ToolOutput;
use rw_types::ToolOutputPart;
use rw_types::hook_contract::HookInput;
use rw_types::hook_contract::HookToolInput;
use rw_types::hook_contract::HookToolResultInput;
use serde_json::Value;
use serde_json::json;
use std::panic::AssertUnwindSafe;
use std::path::Component;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;

pub(super) fn tool_result_output(result: ToolResult) -> ToolOutput {
    if result.data.is_null() && !result.truncated {
        return ToolOutput::Text {
            text: result.content,
        };
    }
    let structured = ToolOutputPart::Structured {
        value: json!({
            "data": result.data,
            "truncated": result.truncated,
        }),
    };
    if result.content.is_empty() {
        ToolOutput::Mixed {
            parts: vec![structured],
        }
    } else {
        ToolOutput::Mixed {
            parts: vec![
                ToolOutputPart::Text {
                    text: result.content,
                },
                structured,
            ],
        }
    }
}

pub(in crate::engine) fn validate_mutation_scope(
    scope: &MutationScope,
) -> Result<(), AgentLoopError> {
    let MutationScope::Paths(paths) = scope else {
        return Ok(());
    };
    if paths.is_empty() {
        return Err(AgentLoopError::ToolContext(
            "mutation scope contained no paths".to_owned(),
        ));
    }
    for path in paths {
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(AgentLoopError::ToolContext(
                "mutation scope contained an unsafe path".to_owned(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct ToolExecutionRuntime {
    pub(super) coordinator: Arc<OrderedOutputCoordinator>,
    pub(super) checkpoints: Arc<dyn MutationCheckpointCoordinator>,
    pub(super) hooks: Arc<HookDispatcher>,
    pub(super) secret_redactor: Arc<dyn SecretRedactor>,
    pub(super) signals: mpsc::UnboundedSender<TurnSignal>,
    pub(super) turn: u64,
    pub(super) subagents: Arc<OrderedSubagentCoordinator>,
    pub(super) tools: Arc<ToolRegistry>,
    pub(super) session_id: SessionId,
}

pub(super) async fn run_deferred_mutating_pre_hook(
    call: &PendingToolCall,
    arguments: &Value,
    cancellation: &CancellationToken,
    runtime: &ToolExecutionRuntime,
) -> Result<(), ToolError> {
    let displayed_arguments = redacted_json(arguments.clone(), runtime.secret_redactor.as_ref());
    let result = dispatch_tool_hook_effect(
        &runtime.hooks,
        HookInput::PreTool(HookToolInput {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: displayed_arguments.clone(),
        }),
        HookEffect::WorkspaceMutating,
        cancellation,
        &runtime.signals,
    )
    .await
    .map_err(|error| match error {
        AgentLoopError::EffectsUnsettled(message) => ToolError::EffectsUnsettled(message),
        error => ToolError::Command(error.to_string()),
    })?;
    report_hook_failures(
        HookEvent::PreTool,
        result.failures(),
        &runtime.signals,
        runtime.secret_redactor.as_ref(),
    );
    if let Some(message) = hook_rejection(result.status(), runtime.secret_redactor.as_ref()) {
        return Err(ToolError::Command(message));
    }
    let HookInput::PreTool(input) = result.input() else {
        unreachable!("dispatcher preserves hook phase")
    };
    if input.name != call.name || input.arguments != displayed_arguments {
        return Err(ToolError::Command(
            "workspace-mutating pre_tool hooks cannot rewrite an authorized invocation".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[tracing::instrument(target = "rw_performance", level = "trace", name = "tool.execute", skip_all, fields(session_id = context.session_id().map_or("", |id| id.0.as_str()))) ]
pub(super) async fn execute_prepared_tool(
    prepared: PreparedToolCall,
    context: ToolContext,
    cancellation: CancellationToken,
    runtime: ToolExecutionRuntime,
) -> (ToolExecution, bool) {
    let (
        call,
        tool,
        arguments,
        mutation_scope,
        semantics,
        authorization,
        deferred_mutating_pre_hook,
    ) = match prepared {
        PreparedToolCall::Execute {
            call,
            tool,
            arguments,
            mutation_scope,
            semantics,
            authorization,
            deferred_mutating_pre_hook,
            ..
        } => (
            call,
            tool,
            arguments,
            mutation_scope,
            semantics,
            authorization,
            deferred_mutating_pre_hook,
        ),
        PreparedToolCall::Complete(execution) => return (execution, false),
    };
    if !matches!(mutation_scope, MutationScope::None)
        && runtime
            .tools
            .session_activity(&runtime.session_id)
            .is_some()
        && !background_control_call(&semantics, &arguments)
    {
        return (
            failed_execution(
                call,
                "workspace mutation is blocked while a background shell process is running",
            ),
            false,
        );
    }
    let checkpoint = if matches!(mutation_scope, MutationScope::None) {
        None
    } else {
        if let Err(error) = validate_mutation_scope(&mutation_scope) {
            return (
                failed_execution(call, format!("checkpoint scope rejected: {error}")),
                false,
            );
        }
        let Some(session_id) = context.session_id() else {
            return (
                failed_execution(call, "tool context is missing a session id"),
                false,
            );
        };
        let begin = runtime
            .checkpoints
            .begin(session_id, runtime.turn, &call.id, &mutation_scope)
            .await;
        match begin {
            Ok(checkpoint) => Some(checkpoint),
            Err(error) => {
                let mut execution =
                    failed_execution(call, format!("checkpoint failed before tool: {error}"));
                if let Err(proof) = runtime.checkpoints.settle_effects().await {
                    execution.unsettled = true;
                    mark_unsettled(&runtime.signals, &cancellation, proof.to_string());
                }
                return (execution, false);
            }
        }
    };
    let output_open = Arc::new(AtomicBool::new(true));
    let sink = Arc::new(OrderedOutputSink {
        index: call.index,
        id: call.id.clone(),
        invocation_id: call.invocation_id.clone(),
        coordinator: Arc::clone(&runtime.coordinator),
        open: output_open.clone(),
        cancellation: cancellation.clone(),
        totals: Mutex::new((0, 0, false)),
    });
    let subagent_events: Arc<dyn SubagentEventSink> = Arc::new(ActorSubagentEventSink {
        index: call.index,
        coordinator: Arc::clone(&runtime.subagents),
        state: Mutex::new(ActorSubagentLifecycleState::default()),
    });
    let progress = InvocationProgress::new(
        runtime.turn,
        call.id.clone(),
        call.invocation_id.clone(),
        runtime.signals.clone(),
        Arc::clone(&runtime.secret_redactor),
    );
    let invocation_context = context
        .with_progress(progress.sink())
        .with_output(sink)
        .with_subagent_event_sink(subagent_events);
    let deferred_pre_result = if deferred_mutating_pre_hook {
        run_deferred_mutating_pre_hook(&call, &arguments, &cancellation, &runtime).await
    } else {
        Ok(())
    };
    let execution_request = PermissionRequest {
        id: call.id.clone(),
        invocation_id: call.invocation_id.clone(),
        tool_name: call.name.clone(),
        arguments: arguments.clone(),
        capabilities: authorization.capabilities.clone(),
        approval_diff: None,
    };
    let diff_revalidation = if let Some(expected) = authorization.approval_diff {
        match tool.approval_preview(&invocation_context, &arguments).await {
            Ok(Some(preview)) => approval_diff(&execution_request, &preview)
                .as_ref()
                .map(diff_binding)
                .filter(|current| current == &expected)
                .map(|_| ())
                .ok_or_else(|| {
                    ToolError::Command(
                        "approved diff is stale; no mutation ran; request a fresh approval"
                            .to_owned(),
                    )
                }),
            Ok(None) => Err(ToolError::Command(
                "approved diff can no longer be reproduced; no mutation ran".to_owned(),
            )),
            Err(error) => Err(ToolError::Command(format!(
                "approved diff could not be revalidated; no mutation ran: {error}"
            ))),
        }
    } else {
        Ok(())
    };
    let revalidation = diff_revalidation.and_then(|()| {
        (PermissionGate::registered_execution_identity(&execution_request, &semantics)
            == authorization.execution_identity)
            .then_some(())
            .ok_or_else(|| {
                ToolError::Command(
                    "approved invocation identity changed; no tool ran; request fresh approval"
                        .to_owned(),
                )
            })
    });
    let result = if let Err(error) = deferred_pre_result {
        Err(error)
    } else if let Err(error) = revalidation {
        Err(error)
    } else if cancellation.is_cancelled() {
        Err(ToolError::Cancelled)
    } else {
        let execution =
            AssertUnwindSafe(tool.execute(&invocation_context, arguments)).catch_unwind();
        tokio::pin!(execution);
        let outcome = tokio::select! {
            outcome = &mut execution => Some(outcome),
            () = cancellation.cancelled() => {
                tokio::time::timeout(TOOL_CANCELLATION_GRACE, &mut execution)
                    .await
                    .ok()
            }
        };
        match outcome {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(ToolError::Command(
                "tool implementation panicked".to_owned(),
            )),
            None => Err(ToolError::Cancelled),
        }
    };
    let settlement = tool.settle_effects().await;
    if let Some(message) = settlement.err().map(|error| error.to_string()).or_else(|| {
        if let Err(ToolError::EffectsUnsettled(message)) = &result {
            Some(message.clone())
        } else {
            None
        }
    }) {
        mark_unsettled(&runtime.signals, &cancellation, message.clone());
        let mut execution = failed_execution(call, message);
        execution.unsettled = true;
        return (execution, true);
    }
    output_open.store(false, Ordering::Release);
    drop(progress);
    let tool_cancelled = matches!(&result, Err(ToolError::Cancelled));
    let (output, is_error) = match result {
        Ok(result) => (tool_result_output(result), false),
        Err(error) => (
            ToolOutput::Text {
                text: error.to_string(),
            },
            true,
        ),
    };
    let mut execution = ToolExecution {
        unsettled: false,
        call,
        output,
        is_error,
    };
    if !cancellation.is_cancelled() {
        execution = apply_post_tool_hook(
            execution,
            runtime.hooks.as_ref(),
            runtime.secret_redactor.as_ref(),
            &cancellation,
            &runtime.signals,
        )
        .await;
    }
    if execution.unsettled {
        return (execution, true);
    }
    let checkpoint_outcome = if tool_cancelled || cancellation.is_cancelled() {
        MutationCheckpointOutcome::Cancelled
    } else if execution.is_error {
        MutationCheckpointOutcome::Failed
    } else {
        MutationCheckpointOutcome::Completed
    };
    if let Some(checkpoint) = &checkpoint {
        let finished = runtime
            .checkpoints
            .finish(checkpoint, checkpoint_outcome)
            .await;
        if let Err(error) = finished {
            execution.output = ToolOutput::Text {
                text: format!("checkpoint finalization failed: {error}"),
            };
            execution.is_error = true;
            execution.unsettled = true;
            mark_unsettled(&runtime.signals, &cancellation, error.to_string());
        }
    }
    if let Err(error) = runtime.checkpoints.settle_effects().await {
        execution.unsettled = true;
        execution.is_error = true;
        mark_unsettled(&runtime.signals, &cancellation, error.to_string());
    }
    (execution, true)
}

pub(super) async fn apply_post_tool_hook(
    mut execution: ToolExecution,
    hooks: &HookDispatcher,
    secret_redactor: &dyn SecretRedactor,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> ToolExecution {
    redact_tool_output(&mut execution.output, secret_redactor);
    let displayed_arguments = redacted_json(
        execution.call.arguments.clone().unwrap_or(Value::Null),
        secret_redactor,
    );
    let post_tool = match dispatch_hook(
        hooks,
        HookInput::PostTool(HookToolResultInput {
            id: execution.call.id.clone(),
            name: execution.call.name.clone(),
            arguments: displayed_arguments,
            output: execution.output.clone(),
            is_error: execution.is_error,
        }),
        cancellation,
        signals,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            execution.output = ToolOutput::Text {
                text: error.to_string(),
            };
            execution.is_error = true;
            execution.unsettled = matches!(error, AgentLoopError::EffectsUnsettled(_));
            return execution;
        }
    };
    report_hook_failures(
        HookEvent::PostTool,
        post_tool.failures(),
        signals,
        secret_redactor,
    );
    if let Some(message) = hook_rejection(post_tool.status(), secret_redactor) {
        execution.output = ToolOutput::Text { text: message };
        execution.is_error = true;
        return execution;
    }
    let HookInput::PostTool(input) = post_tool.input() else {
        unreachable!("dispatcher preserves hook phase")
    };
    execution.output = input.output.clone();
    execution.is_error = input.is_error;
    execution
}
