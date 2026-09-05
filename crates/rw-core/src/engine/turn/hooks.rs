use crate::engine::AgentLoopError;
use crate::engine::pending_event::PendingEvent;
use crate::engine::redaction::SecretRedactor;
use crate::engine::turn::provider_messages::send_event;
use crate::engine::turn::signals::TurnSignal;
use rw_ext::HookDispatchResult;
use rw_ext::HookDispatchStatus;
use rw_ext::HookDispatcher;
use rw_ext::HookEffect;
use rw_ext::HookEvent;
use rw_ext::HookFailure;
use rw_ext::HookFailurePolicy;
use rw_tools::CancellationToken;
use rw_types::hook_contract::HookInput;
use rw_types::hook_contract::HookPermissionDecision;
use tokio::sync::mpsc;

pub(in crate::engine) fn hook_event_name(event: HookEvent) -> &'static str {
    match event {
        HookEvent::SessionStart => "session_start",
        HookEvent::SessionEnd => "session_end",
        HookEvent::UserPromptSubmit => "user_prompt_submit",
        HookEvent::PreTool => "pre_tool",
        HookEvent::PostTool => "post_tool",
        HookEvent::PreCompact => "pre_compact",
        HookEvent::TurnEnd => "turn_end",
        HookEvent::PermissionCheck => "permission_check",
    }
}

pub(super) fn report_hook_failures(
    event: HookEvent,
    failures: &[HookFailure],
    signals: &mpsc::UnboundedSender<TurnSignal>,
    redactor: &dyn SecretRedactor,
) {
    for failure in failures {
        send_event(
            signals,
            PendingEvent::HookFailure {
                event: hook_event_name(event).to_owned(),
                hook_id: failure.hook_id().to_owned(),
                fail_closed: failure.policy() == HookFailurePolicy::FailClosed,
                message: redactor.redact(&failure.error().to_string()),
            },
        );
    }
}

pub(super) fn mark_unsettled(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    cancellation: &CancellationToken,
    message: String,
) {
    cancellation.cancel();
    let _ = signals.send(TurnSignal::EffectsUnsettled { message });
}

pub(super) async fn dispatch_hook(
    dispatcher: &HookDispatcher,
    input: HookInput,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> Result<HookDispatchResult, AgentLoopError> {
    let event = input.event();
    let result = tokio::select! {
        () = cancellation.cancelled() => Err(AgentLoopError::Extension(
            format!("{} hook dispatch cancelled", hook_event_name(event)),
        )),
        result = dispatcher.dispatch(input) => result.map_err(|error| if error.code() == "effects_unsettled" { AgentLoopError::EffectsUnsettled(error.to_string()) } else { AgentLoopError::Extension(error.to_string()) }),
    };
    if let Err(error) = dispatcher.settle_effects(event).await {
        mark_unsettled(signals, cancellation, error.to_string());
        return Err(AgentLoopError::EffectsUnsettled(error.to_string()));
    }
    result
}

pub(super) async fn dispatch_tool_hook_effect(
    dispatcher: &HookDispatcher,
    input: HookInput,
    effect: HookEffect,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> Result<HookDispatchResult, AgentLoopError> {
    let event = input.event();
    let result = tokio::select! {
        () = cancellation.cancelled() => Err(AgentLoopError::Extension(
            format!("{} hook dispatch cancelled", hook_event_name(event)),
        )),
        result = dispatcher.dispatch_tool_effect(input, effect) => result.map_err(|error| if error.code() == "effects_unsettled" { AgentLoopError::EffectsUnsettled(error.to_string()) } else { AgentLoopError::Extension(error.to_string()) }),
    };
    if let Err(error) = dispatcher.settle_effects(event).await {
        mark_unsettled(signals, cancellation, error.to_string());
        return Err(AgentLoopError::EffectsUnsettled(error.to_string()));
    }
    result
}

pub(super) fn hook_rejection(
    status: &HookDispatchStatus,
    redactor: &dyn SecretRedactor,
) -> Option<String> {
    match status {
        HookDispatchStatus::Completed => None,
        HookDispatchStatus::Blocked { hook_id, message } => Some(redactor.redact(&format!(
            "hook `{hook_id}` blocked the operation: {message}"
        ))),
        HookDispatchStatus::FailedClosed { hook_id } => {
            Some(format!("hook `{hook_id}` failed closed"))
        }
    }
}

pub(super) fn permission_hook_override(
    status: &HookDispatchStatus,
    decision: Option<HookPermissionDecision>,
) -> Option<HookPermissionDecision> {
    if matches!(status, HookDispatchStatus::Completed) {
        decision
    } else {
        Some(HookPermissionDecision::Deny)
    }
}
