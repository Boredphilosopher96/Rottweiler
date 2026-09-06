use crate::PermissionApprover;
use crate::PermissionGate;
use crate::PermissionOutcome;
use crate::PermissionRequest;
use crate::engine::AgentLoopError;
use crate::engine::approval_diff;
use crate::engine::diff_binding;
use crate::engine::pending_event::PendingEvent;
use crate::engine::redaction::SecretRedactor;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::hooks::dispatch_hook;
use crate::engine::turn::hooks::dispatch_hook_effect;
use crate::engine::turn::hooks::hook_rejection;
use crate::engine::turn::hooks::permission_hook_override;
use crate::engine::turn::hooks::report_hook_failures;
use crate::engine::turn::provider_messages::send_event;
use crate::engine::turn::redaction::json_contains_redaction;
use crate::engine::turn::redaction::redacted_json;
use crate::engine::turn::redaction::redacted_permission_request;
use crate::engine::turn::signals::TurnSignal;
use async_trait::async_trait;
use rw_ext::HookDispatcher;
use rw_ext::HookEffect;
use rw_ext::HookEvent;
use rw_tools::AskUserInput;
use rw_tools::CancellationToken;
use rw_tools::MutationScope;
use rw_tools::QuestionAsker;
use rw_tools::ToolContext;
use rw_tools::ToolError;
use rw_types::ApprovalBinding;
use rw_types::ApprovalDecision;
use rw_types::SessionMode;
use rw_types::ToolOutput;
use rw_types::UnifiedDiff;
use rw_types::hook_contract::HookInput;
use rw_types::hook_contract::HookPermissionInput;
use rw_types::hook_contract::HookToolInput;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub(super) struct ChannelApprover {
    pub(super) signals: mpsc::UnboundedSender<TurnSignal>,
    pub(super) cancellation: CancellationToken,
}

pub(super) struct RedactingApprover<'a> {
    pub(super) inner: &'a dyn PermissionApprover,
    pub(super) redactor: &'a dyn SecretRedactor,
}

#[async_trait]
impl PermissionApprover for RedactingApprover<'_> {
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision {
        self.inner
            .decide(redacted_permission_request(request, self.redactor))
            .await
    }
}

pub(super) struct ActorQuestionAsker {
    signals: mpsc::UnboundedSender<TurnSignal>,
    cancellation: CancellationToken,
    admission: Arc<tokio::sync::Semaphore>,
}
impl ActorQuestionAsker {
    pub(super) fn new(
        signals: mpsc::UnboundedSender<TurnSignal>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            signals,
            cancellation,
            admission: Arc::new(tokio::sync::Semaphore::new(
                rw_types::question_admission::MAX_PENDING_QUESTION_REQUESTS,
            )),
        }
    }
}

#[async_trait]
impl QuestionAsker for ActorQuestionAsker {
    async fn ask(
        &self,
        request: AskUserInput,
        _cancellation: CancellationToken,
    ) -> Result<String, ToolError> {
        validate_question_input(&request)?;
        let admission = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| ToolError::InvalidInput("question admission is full".into()))?;
        let (respond, receive) = oneshot::channel();
        self.signals
            .send(TurnSignal::Question {
                request,
                respond,
                admission,
            })
            .map_err(|_| ToolError::Cancelled)?;
        tokio::select! {
            () = self.cancellation.cancelled() => Err(ToolError::Cancelled),
            response = receive => response.map_err(|_| ToolError::Cancelled)?,
        }
    }
}

#[async_trait]
impl PermissionApprover for ChannelApprover {
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision {
        let (respond, receive) = oneshot::channel();
        if self
            .signals
            .send(TurnSignal::Approval { request, respond })
            .is_err()
        {
            return ApprovalDecision::Deny;
        }
        tokio::select! {
            () = self.cancellation.cancelled() => ApprovalDecision::Deny,
            decision = receive => decision.unwrap_or(ApprovalDecision::Deny),
        }
    }
}

#[derive(Clone)]
pub(super) struct PendingToolCall {
    pub(super) id: String,
    pub(super) invocation_id: rw_types::ToolInvocationId,
    pub(super) name: String,
    pub(super) arguments: Option<Value>,
    pub(super) index: usize,
}

pub(in crate::engine) struct ToolExecution {
    pub(super) presentation: Option<rw_tools::ToolPresentationPlan>,
    pub(super) unsettled: bool,
    pub(super) call: PendingToolCall,
    pub(super) output: ToolOutput,
    pub(super) is_error: bool,
}

pub(super) struct AuthorizedToolBinding {
    pub(super) approval_diff: Option<ApprovalBinding>,
    pub(super) execution_identity: String,
    pub(super) capabilities: Vec<rw_types::ToolCapability>,
}

pub(super) enum PreparedToolCall {
    Execute {
        call: PendingToolCall,
        tool: Arc<dyn rw_tools::Tool>,
        arguments: Value,
        read_only: bool,
        mutation_scope: MutationScope,
        semantics: Box<rw_tools::ToolInvocationSemantics>,
        authorization: AuthorizedToolBinding,
        deferred_mutating_pre_hook: bool,
    },
    Complete(ToolExecution),
}

impl PreparedToolCall {
    pub(super) fn call(&self) -> &PendingToolCall {
        match self {
            Self::Execute { call, .. } | Self::Complete(ToolExecution { call, .. }) => call,
        }
    }
}

pub(super) fn failed_execution(call: PendingToolCall, message: impl Into<String>) -> ToolExecution {
    ToolExecution {
        presentation: None,
        unsettled: false,
        call,
        output: ToolOutput::Text {
            text: message.into(),
        },
        is_error: true,
    }
}

pub(super) struct ResolvedToolSecurity {
    pub(super) tool: Arc<dyn rw_tools::Tool>,
    pub(super) capabilities: Vec<rw_types::ToolCapability>,
    pub(super) mutation_scope: MutationScope,
    pub(super) semantics: rw_tools::ToolInvocationSemantics,
    pub(super) read_only: bool,
}

pub(super) fn resolve_tool_security(
    config: &SessionActorConfig,
    name: &str,
    arguments: &Value,
) -> Option<ResolvedToolSecurity> {
    let tool = config.tools.resolve(name)?;
    let semantics = config.tools.invocation_semantics(name, arguments).ok()??;
    let mutation_scope = semantics.mutation_scope.clone();
    let mut capabilities = tool
        .invocation_capabilities(arguments)
        .ok()?
        .capabilities()
        .to_vec();
    if !matches!(mutation_scope, MutationScope::None)
        && !capabilities.contains(&rw_types::ToolCapability::WriteFilesystem)
    {
        capabilities.push(rw_types::ToolCapability::WriteFilesystem);
    }
    let read_only = tool.parallel_safe(arguments);
    Some(ResolvedToolSecurity {
        tool,
        capabilities,
        mutation_scope,
        semantics,
        read_only,
    })
}

pub(super) fn widen_security_for_hooks(
    mut security: ResolvedToolSecurity,
    hooks: &HookDispatcher,
    tool_name: &str,
) -> (ResolvedToolSecurity, bool) {
    for event in [HookEvent::PreTool, HookEvent::PostTool] {
        for capability in hooks.required_tool_capabilities(event, tool_name) {
            if !security.capabilities.contains(&capability) {
                security.capabilities.push(capability);
            }
        }
    }
    let deferred_mutating_pre_hook =
        hooks.has_workspace_mutating_tool_hook(HookEvent::PreTool, tool_name);
    let mutating_post_hook = hooks.has_workspace_mutating_tool_hook(HookEvent::PostTool, tool_name);
    if deferred_mutating_pre_hook || mutating_post_hook {
        security.mutation_scope = MutationScope::OpaqueWorkspace;
        security.read_only = false;
        if !security
            .capabilities
            .contains(&rw_types::ToolCapability::WriteFilesystem)
        {
            security
                .capabilities
                .push(rw_types::ToolCapability::WriteFilesystem);
        }
    }
    (security, deferred_mutating_pre_hook)
}

pub(super) fn background_control_call(
    semantics: &rw_tools::ToolInvocationSemantics,
    arguments: &Value,
) -> bool {
    semantics.behavior == rw_tools::ToolBehavior::BackgroundControl
        || (semantics.behavior == rw_tools::ToolBehavior::Shell
            && arguments.get("run_in_background").and_then(Value::as_bool) == Some(true))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn authorize_tool_call(
    budget: &mut super::tool_admission::PendingToolBudget,
    turn: u64,
    call: &PendingToolCall,
    arguments: &Value,
    capabilities: Vec<rw_types::ToolCapability>,
    semantics: &rw_tools::ToolInvocationSemantics,
    tool: &Arc<dyn rw_tools::Tool>,
    context: &ToolContext,
    config: &SessionActorConfig,
    approver: &dyn PermissionApprover,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    mode: SessionMode,
) -> Result<AuthorizedToolBinding, String> {
    let mut request = PermissionRequest {
        id: call.id.clone(),
        invocation_id: call.invocation_id.clone(),
        tool_name: call.name.clone(),
        arguments: arguments.clone(),
        capabilities,
        approval_diff: None,
    };
    request.approval_diff = current_approval_diff(tool, context, &request).await?;
    budget.approval_payload(&request, 1)?;
    let authorization = AuthorizedToolBinding {
        approval_diff: request.approval_diff.as_ref().map(diff_binding),
        execution_identity: PermissionGate::registered_execution_identity(&request, semantics),
        capabilities: request.capabilities.clone(),
    };
    let displayed = redacted_permission_request(request.clone(), config.secret_redactor.as_ref());
    // Preview and approval publications may each retain their own displayed copy.
    budget.approval_payload(&displayed, 2)?;
    if let Some(diff) = displayed.approval_diff.clone() {
        send_event(
            signals,
            PendingEvent::ToolDiffReady {
                turn,
                id: call.id.clone(),
                invocation_id: call.invocation_id.clone(),
                diff,
            },
        );
    }
    let permission_hook = dispatch_hook(
        &config.hooks,
        HookInput::PermissionCheck(HookPermissionInput {
            id: displayed.id.clone(),
            name: displayed.tool_name.clone(),
            arguments: displayed.arguments.clone(),
            capabilities: displayed.capabilities.clone(),
        }),
        cancellation,
        signals,
    )
    .await
    .map_err(|error| error.to_string())?;
    report_hook_failures(
        HookEvent::PermissionCheck,
        permission_hook.failures(),
        signals,
        config.secret_redactor.as_ref(),
    );
    let redacting_approver = RedactingApprover {
        inner: approver,
        redactor: config.secret_redactor.as_ref(),
    };
    let permission = config
        .permissions
        .authorize_registered_in_mode(
            request,
            semantics,
            &redacting_approver,
            permission_hook_override(permission_hook.status(), permission_hook.permission()),
            mode,
        )
        .await;
    match permission {
        PermissionOutcome::Allowed => Ok(authorization),
        PermissionOutcome::Denied => Err(format!("permission denied for tool `{}`", call.name)),
        PermissionOutcome::RememberedApprovalUnavailable => Err(format!(
            "remembered_permission_unavailable: tool `{}` cannot safely remember this invocation; choose allow once",
            call.name
        )),
    }
}

pub(in crate::engine) async fn current_approval_diff(
    tool: &Arc<dyn rw_tools::Tool>,
    context: &ToolContext,
    request: &PermissionRequest,
) -> Result<Option<UnifiedDiff>, String> {
    let preview = tool
        .approval_preview(context, &request.arguments)
        .await
        .map_err(|error| format!("could not prepare approval preview: {error}"))?;
    Ok(preview
        .as_ref()
        .and_then(|preview| approval_diff(request, preview)))
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(target = "rw_performance", level = "trace", name = "tool.prepare", skip_all, fields(session_id = config.session_id.0.as_str(), turn, tool_call_id = call.id.as_str()))]
pub(super) async fn prepare_tool_call(
    turn: u64,
    mut call: PendingToolCall,
    config: &SessionActorConfig,
    approver: &dyn PermissionApprover,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    context: &ToolContext,
    mode: SessionMode,
    admission: &mut super::tool_admission::PendingToolBudget,
    displayed_arguments: Value,
) -> PreparedToolCall {
    send_event(
        signals,
        PendingEvent::ToolCallStarted {
            turn,
            id: call.id.clone(),
            invocation_id: call.invocation_id.clone(),
            name: call.name.clone(),
            arguments: displayed_arguments.clone(),
            index: call.index,
        },
    );
    let Some(arguments) = call.arguments.clone() else {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "provider did not finish tool-call arguments",
        ));
    };
    let Some(initial_security) = resolve_tool_security(config, &call.name, &arguments) else {
        let name = call.name.clone();
        return PreparedToolCall::Complete(failed_execution(
            call,
            format!("unknown tool `{name}`"),
        ));
    };
    let (initial_security, _) =
        widen_security_for_hooks(initial_security, &config.hooks, &call.name);
    let background_control = background_control_call(&initial_security.semantics, &arguments);
    if background_control && !matches!(initial_security.mutation_scope, MutationScope::None) {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "background commands cannot run with workspace-mutating hooks",
        ));
    }
    if config.tools.session_activity(&config.session_id).is_some()
        && !matches!(initial_security.mutation_scope, MutationScope::None)
        && !background_control
    {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "workspace mutation is blocked while a background shell process is running",
        ));
    }
    let mut authorization = match authorize_tool_call(
        admission,
        turn,
        &call,
        &arguments,
        initial_security.capabilities.clone(),
        &initial_security.semantics,
        &initial_security.tool,
        context,
        config,
        approver,
        cancellation,
        signals,
        mode,
    )
    .await
    {
        Ok(binding) => binding,
        Err(message) => return PreparedToolCall::Complete(failed_execution(call, message)),
    };
    let original_name = call.name.clone();
    let original_arguments = arguments.clone();
    let pre_tool = match dispatch_hook_effect(
        &config.hooks,
        HookInput::PreTool(HookToolInput {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: displayed_arguments.clone(),
        }),
        HookEffect::ReadOnly,
        cancellation,
        signals,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            let mut execution = failed_execution(call, error.to_string());
            execution.unsettled = matches!(error, AgentLoopError::EffectsUnsettled(_));
            return PreparedToolCall::Complete(execution);
        }
    };
    report_hook_failures(
        HookEvent::PreTool,
        pre_tool.failures(),
        signals,
        config.secret_redactor.as_ref(),
    );
    if let Some(message) = hook_rejection(pre_tool.status(), config.secret_redactor.as_ref()) {
        return PreparedToolCall::Complete(failed_execution(call, message));
    }
    let HookInput::PreTool(input) = pre_tool.input() else {
        unreachable!("dispatcher preserves hook phase")
    };
    call.name = input.name.clone();
    let hook_arguments = input.arguments.clone();
    let arguments = if hook_arguments
        == redacted_json(original_arguments.clone(), config.secret_redactor.as_ref())
    {
        original_arguments.clone()
    } else if json_contains_redaction(&hook_arguments) {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "pre_tool hook cannot execute a rewritten redacted placeholder",
        ));
    } else {
        hook_arguments
    };
    if let Err(error) = admission.replace(call.arguments.as_ref(), &arguments) {
        return PreparedToolCall::Complete(failed_execution(call, error));
    }
    call.arguments = Some(arguments.clone());
    let Some(security) = resolve_tool_security(config, &call.name, &arguments) else {
        let name = call.name.clone();
        return PreparedToolCall::Complete(failed_execution(
            call,
            format!("unknown tool `{name}`"),
        ));
    };
    let (security, deferred_mutating_pre_hook) =
        widen_security_for_hooks(security, &config.hooks, &call.name);
    let background_control = background_control_call(&security.semantics, &arguments);
    if background_control && !matches!(security.mutation_scope, MutationScope::None) {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "background commands cannot run with workspace-mutating hooks",
        ));
    }
    if config.tools.session_activity(&config.session_id).is_some()
        && !matches!(security.mutation_scope, MutationScope::None)
        && !background_control
    {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "workspace mutation is blocked while a background shell process is running",
        ));
    }
    if call.name != original_name || arguments != original_arguments {
        authorization = match authorize_tool_call(
            admission,
            turn,
            &call,
            &arguments,
            security.capabilities.clone(),
            &security.semantics,
            &security.tool,
            context,
            config,
            approver,
            cancellation,
            signals,
            mode,
        )
        .await
        {
            Ok(binding) => binding,
            Err(message) => return PreparedToolCall::Complete(failed_execution(call, message)),
        };
    }
    PreparedToolCall::Execute {
        call,
        tool: security.tool,
        arguments,
        read_only: security.read_only,
        mutation_scope: security.mutation_scope,
        semantics: Box::new(security.semantics),
        authorization,
        deferred_mutating_pre_hook,
    }
}

fn validate_question_input(request: &AskUserInput) -> Result<(), ToolError> {
    use rw_types::question_admission::{MAX_QUESTION_SET_BYTES, MAX_QUESTION_SET_PREPARED_BYTES};
    if request.options.len() > rw_types::question_admission::MAX_PENDING_QUESTION_REQUESTS {
        return Err(ToolError::InvalidInput(
            "question option count exceeds admission".into(),
        ));
    }
    let retained = request.options.iter().fold(
        request.question.capacity().checked_add(
            request
                .options
                .capacity()
                .saturating_mul(std::mem::size_of::<String>()),
        ),
        |total, option| {
            total.and_then(|bytes| bytes.checked_add(option.capacity().saturating_mul(2)))
        },
    );
    if request.question.len() > MAX_QUESTION_SET_BYTES
        || retained.is_none_or(|bytes| bytes > MAX_QUESTION_SET_PREPARED_BYTES / 2)
    {
        return Err(ToolError::InvalidInput(
            "question payload exceeds admission".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod question_admission_tests {
    use super::{ActorQuestionAsker, QuestionAsker};
    use rw_tools::{AskUserInput, CancellationToken, ToolError};
    #[tokio::test]
    async fn dropped_question_waiters_do_not_release_queued_request_admission() {
        use std::future::Future;
        let (signals, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let asker = ActorQuestionAsker::new(signals, CancellationToken::default());
        let request = || AskUserInput {
            question: "choice".into(),
            options: Vec::new(),
            allow_free_text: true,
        };
        let mut pending = (0..rw_types::question_admission::MAX_PENDING_QUESTION_REQUESTS)
            .map(|_| Box::pin(asker.ask(request(), CancellationToken::default())))
            .collect::<Vec<_>>();
        for question in &mut pending {
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(question.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
        }
        drop(pending);
        assert!(matches!(
            asker.ask(request(), CancellationToken::default()).await,
            Err(ToolError::InvalidInput(_))
        ));
        drop(receive.try_recv().expect("owned queued request"));
        let mut admitted = Box::pin(asker.ask(request(), CancellationToken::default()));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(admitted.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        assert_eq!(asker.admission.available_permits(), 0);
    }

    #[tokio::test]
    async fn oversized_questions_fail_before_entering_the_actor_signal_queue() {
        let (signals, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let asker = ActorQuestionAsker::new(signals, CancellationToken::default());
        let error = asker
            .ask(
                AskUserInput {
                    question: "x".repeat(rw_types::question_admission::MAX_QUESTION_SET_BYTES + 1),
                    options: vec![],
                    allow_free_text: true,
                },
                CancellationToken::default(),
            )
            .await
            .expect_err("typed admission error");
        assert!(matches!(error, ToolError::InvalidInput(_)));
        assert!(receive.try_recv().is_err());
    }
}
