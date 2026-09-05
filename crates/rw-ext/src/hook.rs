use std::collections::BTreeMap;
use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::FutureExt;
use rw_tools::CancellationToken;
use rw_types::ToolCapability;
use thiserror::Error;
use tokio::time::Instant;

const HOOK_PHASE_TIMEOUT: Duration = Duration::from_millis(HOOK_PHASE_TIMEOUT_MS);
const HOOK_SETTLEMENT_TIMEOUT: Duration = Duration::from_millis(HOOK_SETTLEMENT_TIMEOUT_MS);

use rw_types::hook_contract::{
    HOOK_PHASE_TIMEOUT_MS, HOOK_SETTLEMENT_TIMEOUT_MS, MAX_HOOK_DIAGNOSTIC_BYTES,
    MAX_HOOKS_PER_EVENT,
};
pub use rw_types::hook_contract::{
    HookClass, HookDirective, HookEvent, HookFailurePolicy, HookInput, HookPermissionDecision,
    HookTransform,
};
mod settlement;
use settlement::HookRuntime;

/// Filesystem effect declared by a hook before it becomes eligible to run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HookEffect {
    #[default]
    ReadOnly,
    WorkspaceMutating,
}

/// Public registration metadata shared by in-process and RPC hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookRegistration {
    id: String,
    event: HookEvent,
    priority: i32,
    class: HookClass,
    failure_policy: HookFailurePolicy,
    timeout: Duration,
    effect: HookEffect,
    applicable_tools: Vec<String>,
    required_capabilities: Vec<ToolCapability>,
}

impl HookRegistration {
    /// Creates a registration. Lower priorities run first; equal priorities are
    /// ordered by ID.
    #[must_use]
    pub fn new(id: impl Into<String>, event: HookEvent, class: HookClass) -> Self {
        Self {
            id: id.into(),
            event,
            priority: 0,
            class,
            failure_policy: if class == HookClass::Observer {
                HookFailurePolicy::FailOpen
            } else {
                HookFailurePolicy::FailClosed
            },
            timeout: Duration::from_secs(5),
            effect: HookEffect::ReadOnly,
            applicable_tools: Vec::new(),
            required_capabilities: Vec::new(),
        }
    }

    #[must_use]
    pub const fn class(&self) -> HookClass {
        self.class
    }

    /// Sets the deterministic ordering priority.
    #[must_use]
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Sets failure handling for errors and bridge-reported timeouts.
    #[must_use]
    pub fn with_failure_policy(mut self, failure_policy: HookFailurePolicy) -> Self {
        self.failure_policy = failure_policy;
        self
    }

    /// Sets the maximum duration of one handler invocation.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Declares whether this hook may mutate the workspace.
    #[must_use]
    pub const fn with_effect(mut self, effect: HookEffect) -> Self {
        self.effect = effect;
        self
    }

    /// Restricts this registration to exact canonical tool names. An empty
    /// list applies to every tool.
    #[must_use]
    pub fn with_applicable_tools(
        mut self,
        tools: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.applicable_tools = tools.into_iter().map(Into::into).collect();
        self.applicable_tools.sort();
        self.applicable_tools.dedup();
        self
    }

    /// Declares capabilities consumed by the hook itself. The engine merges
    /// these into the matched tool request before authorization.
    #[must_use]
    pub fn with_required_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = ToolCapability>,
    ) -> Self {
        self.required_capabilities.clear();
        for capability in capabilities {
            if !self.required_capabilities.contains(&capability) {
                self.required_capabilities.push(capability);
            }
        }
        self
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn event(&self) -> HookEvent {
        self.event
    }

    #[must_use]
    pub const fn priority(&self) -> i32 {
        self.priority
    }

    #[must_use]
    pub const fn failure_policy(&self) -> HookFailurePolicy {
        self.failure_policy
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub const fn effect(&self) -> HookEffect {
        self.effect
    }

    #[must_use]
    pub fn applicable_tools(&self) -> &[String] {
        &self.applicable_tools
    }

    #[must_use]
    pub fn applies_to_tool(&self, name: &str) -> bool {
        self.applicable_tools.is_empty()
            || self
                .applicable_tools
                .binary_search_by(|tool| tool.as_str().cmp(name))
                .is_ok()
    }

    #[must_use]
    pub fn required_capabilities(&self) -> &[ToolCapability] {
        &self.required_capabilities
    }
}

/// Immutable input to one hook. A replacement returned by one hook becomes the
/// payload observed by the next hook.
#[derive(Clone, Copy)]
pub struct HookInvocation<'a> {
    input: &'a HookInput,
    cancellation: &'a CancellationToken,
}

impl HookInvocation<'_> {
    #[must_use]
    pub const fn event(&self) -> HookEvent {
        self.input.event()
    }

    #[must_use]
    pub const fn input(&self) -> &HookInput {
        self.input
    }

    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        self.cancellation
    }
}

/// A handler-reported failure, including timeout errors produced by bridges.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{code}: {message}")]
pub struct HookError {
    code: String,
    message: String,
}

impl HookError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: bounded_diagnostic(code.into()),
            message: bounded_diagnostic(message.into()),
        }
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Common handler interface used by built-ins and extensions.
#[async_trait]
pub trait HookHandler: Send + Sync {
    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError>;

    /// Waits for every effect that can outlive the cancellable invocation future.
    /// The dispatcher drops that future on cancellation before requesting this proof.
    async fn settle_effects(&self) -> Result<(), HookError>;
}

/// One recorded handler failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookFailure {
    hook_id: String,
    policy: HookFailurePolicy,
    error: HookError,
}

impl HookFailure {
    #[must_use]
    pub fn hook_id(&self) -> &str {
        &self.hook_id
    }

    #[must_use]
    pub const fn policy(&self) -> HookFailurePolicy {
        self.policy
    }

    #[must_use]
    pub const fn error(&self) -> &HookError {
        &self.error
    }
}

/// Terminal disposition of a hook pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookDispatchStatus {
    Completed,
    Blocked { hook_id: String, message: String },
    FailedClosed { hook_id: String },
}

/// Full deterministic result, including failures that were allowed open.
#[derive(Clone, Debug, PartialEq)]
pub struct HookDispatchResult {
    input: HookInput,
    permission: Option<HookPermissionDecision>,
    status: HookDispatchStatus,
    failures: Vec<HookFailure>,
}

impl HookDispatchResult {
    #[must_use]
    pub const fn input(&self) -> &HookInput {
        &self.input
    }

    #[must_use]
    pub const fn permission(&self) -> Option<HookPermissionDecision> {
        self.permission
    }

    #[must_use]
    pub const fn status(&self) -> &HookDispatchStatus {
        &self.status
    }

    #[must_use]
    pub fn failures(&self) -> &[HookFailure] {
        &self.failures
    }

    #[must_use]
    pub const fn completed(&self) -> bool {
        matches!(self.status, HookDispatchStatus::Completed)
    }
}

/// Hook registration failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum HookRegistrationError {
    #[error("hook ID must not be empty or contain control characters")]
    InvalidId,
    #[error("hook applicable tool names must use canonical lowercase snake_case")]
    InvalidToolName,
    #[error("hook class, effect and failure policy do not form a valid registration")]
    InvalidClass,
    #[error("hook count exceeds the phase limit")]
    Capacity,
    #[error("hook `{id}` is already registered for {event:?}")]
    Duplicate { event: HookEvent, id: String },
}

#[derive(Clone)]
struct RegisteredHook {
    registration: HookRegistration,
    handler: Arc<dyn HookHandler>,
    runtime: Arc<HookRuntime>,
}

async fn invoke_registered_hook(
    registered: &RegisteredHook,
    input: &HookInput,
    deadline: Instant,
    settlement_deadline: Instant,
) -> Result<HookDirective, HookError> {
    let invocation_deadline = deadline.min(Instant::now() + registered.registration.timeout());
    let mut owner = registered
        .runtime
        .admit(Arc::clone(&registered.handler), invocation_deadline)
        .await?;
    let (result, cleanup_deadline) = {
        let invocation = HookInvocation {
            input,
            cancellation: &owner.cancellation,
        };
        let invoked =
            AssertUnwindSafe(async { registered.handler.invoke(invocation).await }).catch_unwind();
        tokio::pin!(invoked);
        tokio::select! {
            result = &mut invoked => (
                result.unwrap_or_else(|_| Err(HookError::new("panic", "hook implementation panicked"))),
                settlement_deadline.min(Instant::now() + HOOK_SETTLEMENT_TIMEOUT),
            ),
            () = tokio::time::sleep_until(invocation_deadline) => {
                let cleanup_deadline = settlement_deadline.min(Instant::now() + HOOK_SETTLEMENT_TIMEOUT);
                owner.cancellation.cancel();
                (Err(HookError::new("timeout", "hook invocation deadline elapsed")), cleanup_deadline)
            }
        }
    };
    let cleanup = owner.finish().ok_or_else(|| {
        HookError::new(
            "effects_unsettled",
            "hook invocation has no settlement owner",
        )
    })?;
    match tokio::time::timeout_at(cleanup_deadline, cleanup).await {
        Ok(Ok(())) => result,
        Ok(Err(error)) => {
            registered.runtime.close_admission();
            Err(error)
        }
        Err(_) => {
            registered.runtime.close_admission();
            Err(HookError::new(
                "effects_unsettled",
                "hook effect settlement deadline elapsed",
            ))
        }
    }
}

/// Deterministic request/response hook dispatcher.
///
/// Hooks execute serially by `(class, priority, id)`. This makes the observable
/// pipeline independent of extension discovery order and asynchronous runtime
/// scheduling.
#[derive(Clone, Default)]
pub struct HookDispatcher {
    hooks: BTreeMap<HookEvent, Vec<RegisteredHook>>,
}

impl HookDispatcher {
    /// Joins external cleanup after a caller drops a dispatch future.
    ///
    /// # Errors
    /// Returns the first failed effect proof after checking every registered handler.
    pub async fn settle_effects(&self, event: HookEvent) -> Result<(), HookError> {
        let deadline = Instant::now() + HOOK_SETTLEMENT_TIMEOUT;
        let mut failure = None;
        if let Some(hooks) = self.hooks.get(&event) {
            for hook in hooks {
                if let Err(error) = hook.runtime.settle(deadline).await {
                    failure.get_or_insert(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a hook through the common built-in/extension API.
    ///
    /// # Errors
    ///
    /// Returns [`HookRegistrationError::InvalidId`] for an unusable ID, or
    /// [`HookRegistrationError::Duplicate`] for an existing `(event, id)`.
    pub fn register<Handler>(
        &mut self,
        registration: HookRegistration,
        handler: Handler,
    ) -> Result<(), HookRegistrationError>
    where
        Handler: HookHandler + 'static,
    {
        self.register_shared(registration, Arc::new(handler))
    }

    /// Registers a shared hook handler, as used by forwarding bridges.
    ///
    /// # Errors
    ///
    /// Returns [`HookRegistrationError::InvalidId`] for an unusable ID, or
    /// [`HookRegistrationError::Duplicate`] for an existing `(event, id)`.
    pub fn register_shared(
        &mut self,
        registration: HookRegistration,
        handler: Arc<dyn HookHandler>,
    ) -> Result<(), HookRegistrationError> {
        validate_id(registration.id())?;
        if (registration.class() == HookClass::Transform
            && !registration.event().accepts_transform())
            || (registration.class() == HookClass::Policy
                && registration.failure_policy() != HookFailurePolicy::FailClosed)
            || (registration.class() == HookClass::Observer
                && registration.effect() != HookEffect::ReadOnly)
            || (registration.effect() == HookEffect::WorkspaceMutating
                && !matches!(
                    registration.event(),
                    HookEvent::PreTool | HookEvent::PostTool
                ))
            || registration.timeout().is_zero()
            || registration.timeout() > Duration::from_mins(10)
        {
            return Err(HookRegistrationError::InvalidClass);
        }
        if registration.applicable_tools().iter().any(|name| {
            name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        }) {
            return Err(HookRegistrationError::InvalidToolName);
        }
        let event = registration.event();
        let event_hooks = self.hooks.entry(event).or_default();
        if event_hooks
            .iter()
            .any(|registered| registered.registration.id() == registration.id())
        {
            return Err(HookRegistrationError::Duplicate {
                event,
                id: registration.id,
            });
        }
        if event_hooks.len() >= MAX_HOOKS_PER_EVENT {
            return Err(HookRegistrationError::Capacity);
        }
        event_hooks.push(RegisteredHook {
            registration,
            handler,
            runtime: Arc::new(HookRuntime::default()),
        });
        event_hooks.sort_by(|left, right| {
            left.registration
                .class()
                .cmp(&right.registration.class())
                .then_with(|| {
                    left.registration
                        .priority()
                        .cmp(&right.registration.priority())
                })
                .then_with(|| left.registration.id().cmp(right.registration.id()))
        });
        Ok(())
    }

    /// Returns registrations in their exact execution order.
    pub fn registrations(
        &self,
        event: HookEvent,
    ) -> impl ExactSizeIterator<Item = &HookRegistration> {
        self.hooks
            .get(&event)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|registered| &registered.registration)
    }

    /// Reports whether a workspace-mutating registration can apply to this
    /// exact canonical tool name.
    #[must_use]
    pub fn has_workspace_mutating_tool_hook(&self, event: HookEvent, tool_name: &str) -> bool {
        self.registrations(event).any(|registration| {
            registration.effect() == HookEffect::WorkspaceMutating
                && registration.applies_to_tool(tool_name)
        })
    }

    /// Returns the deduplicated capabilities consumed by matching hooks for
    /// one event and canonical tool name.
    #[must_use]
    pub fn required_tool_capabilities(
        &self,
        event: HookEvent,
        tool_name: &str,
    ) -> Vec<ToolCapability> {
        let mut capabilities = Vec::new();
        for registration in self
            .registrations(event)
            .filter(|registration| registration.applies_to_tool(tool_name))
        {
            for capability in registration.required_capabilities() {
                if !capabilities.contains(capability) {
                    capabilities.push(capability.clone());
                }
            }
        }
        capabilities
    }

    /// Dispatches one filesystem-effect phase for the input's tool.
    ///
    /// # Errors
    /// Rejects oversized input and returns an error if physical effects cannot settle.
    pub async fn dispatch_tool_effect(
        &self,
        input: HookInput,
        effect: HookEffect,
    ) -> Result<HookDispatchResult, HookError> {
        self.dispatch_selected(input, Some(effect)).await
    }

    /// Executes transforms, policies and observers under one fixed phase deadline.
    ///
    /// # Errors
    /// Rejects oversized input and returns an error if physical effects cannot settle.
    pub async fn dispatch(&self, input: HookInput) -> Result<HookDispatchResult, HookError> {
        self.dispatch_selected(input, None).await
    }

    async fn dispatch_selected(
        &self,
        input: HookInput,
        effect: Option<HookEffect>,
    ) -> Result<HookDispatchResult, HookError> {
        let event = input.event();
        let mut result = HookDispatchResult {
            input,
            permission: None,
            status: HookDispatchStatus::Completed,
            failures: Vec::new(),
        };
        let Some(hooks) = self.hooks.get(&event) else {
            return Ok(result);
        };
        let budget = hooks
            .iter()
            .filter(|hook| effect.is_none_or(|effect| effect == hook.registration.effect()))
            .map(|hook| hook.registration.timeout())
            .max()
            .unwrap_or(HOOK_PHASE_TIMEOUT);
        let deadline = Instant::now() + budget;
        let settlement_deadline = deadline + HOOK_SETTLEMENT_TIMEOUT;
        let mut input_checked = false;
        for registered in hooks {
            let registration = &registered.registration;
            if effect.is_some_and(|effect| effect != registration.effect())
                || result
                    .input
                    .tool_name()
                    .is_some_and(|name| !registration.applies_to_tool(name))
            {
                continue;
            }
            if !input_checked {
                check_size(&result.input)?;
                input_checked = true;
            }
            let invoked = if Instant::now() >= deadline {
                Err(HookError::new(
                    "phase_timeout",
                    "aggregate hook phase deadline elapsed",
                ))
            } else {
                invoke_registered_hook(registered, &result.input, deadline, settlement_deadline)
                    .await
            };
            let outcome = invoked.and_then(|directive| {
                apply_directive(registration.class(), &mut result, directive)
            });
            if let Err(error) = outcome {
                if error.code() == "effects_unsettled" {
                    return Err(error);
                }
                let policy = registration.failure_policy();
                let failed_closed = policy == HookFailurePolicy::FailClosed
                    || (error.code() == "phase_timeout"
                        && registration.class() != HookClass::Observer);
                result.failures.push(HookFailure {
                    hook_id: registration.id.clone(),
                    policy,
                    error,
                });
                if failed_closed {
                    result.status = HookDispatchStatus::FailedClosed {
                        hook_id: registration.id.clone(),
                    };
                    return Ok(result);
                }
            }
            if matches!(result.status, HookDispatchStatus::Blocked { .. }) {
                if let HookDispatchStatus::Blocked { hook_id, .. } = &mut result.status {
                    hook_id.clone_from(&registration.id);
                }
                return Ok(result);
            }
        }
        Ok(result)
    }
}

fn apply_directive(
    class: HookClass,
    result: &mut HookDispatchResult,
    directive: HookDirective,
) -> Result<(), HookError> {
    check_size(&directive)?;
    match directive {
        HookDirective::Continue {} => Ok(()),
        HookDirective::Transform { change } if class == HookClass::Transform => {
            let mut candidate = result.input.clone();
            candidate
                .apply(change)
                .map_err(|message| HookError::new("invalid_directive", message))?;
            check_size(&candidate)?;
            result.input = candidate;
            Ok(())
        }
        HookDirective::Permission { value }
            if class == HookClass::Policy && result.input.event() == HookEvent::PermissionCheck =>
        {
            result.permission = Some(
                result
                    .permission
                    .map_or(value, |earlier| earlier.max(value)),
            );
            if value == HookPermissionDecision::Deny {
                result.status = HookDispatchStatus::Blocked {
                    hook_id: String::new(),
                    message: "permission hook denied the invocation".to_owned(),
                };
            }
            Ok(())
        }
        HookDirective::Block { message } if class == HookClass::Policy => {
            if message.is_empty()
                || message.len() > MAX_HOOK_DIAGNOSTIC_BYTES
                || message
                    .chars()
                    .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
            {
                return Err(HookError::new(
                    "invalid_directive",
                    "hook block message is invalid",
                ));
            }
            result.status = HookDispatchStatus::Blocked {
                hook_id: String::new(),
                message,
            };
            Ok(())
        }
        _ => Err(HookError::new(
            "invalid_directive",
            "hook decision is not legal for its phase and class",
        )),
    }
}

fn check_size(value: &impl serde::Serialize) -> Result<(), HookError> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|bytes| *bytes <= rw_plugin_protocol::MAX_HOOK_PAYLOAD_BYTES)
                .ok_or_else(|| std::io::Error::other("hook payload exceeds its byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), value)
        .map_err(|_| HookError::new("payload_limit", "hook payload exceeds its byte limit"))
}

fn bounded_diagnostic(mut value: String) -> String {
    let mut end = value.len().min(MAX_HOOK_DIAGNOSTIC_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn validate_id(id: &str) -> Result<(), HookRegistrationError> {
    if id.is_empty() || id.chars().any(char::is_control) {
        Err(HookRegistrationError::InvalidId)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
