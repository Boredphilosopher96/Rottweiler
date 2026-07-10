use std::collections::BTreeMap;
use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::Value;
use thiserror::Error;

/// Stable catalog of request/response hook points.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreTool,
    PostTool,
    PreCompact,
    TurnEnd,
    PermissionCheck,
}

/// Whether a handler failure permits dispatch to continue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookFailurePolicy {
    FailOpen,
    FailClosed,
}

/// Public registration metadata shared by in-process and future RPC hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookRegistration {
    id: String,
    event: HookEvent,
    priority: i32,
    failure_policy: HookFailurePolicy,
    timeout: Duration,
}

impl HookRegistration {
    /// Creates a registration. Lower priorities run first; equal priorities are
    /// ordered by ID.
    #[must_use]
    pub fn new(id: impl Into<String>, event: HookEvent) -> Self {
        Self {
            id: id.into(),
            event,
            priority: 0,
            failure_policy: HookFailurePolicy::FailOpen,
            timeout: Duration::from_secs(5),
        }
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
}

/// Immutable input to one hook. A replacement returned by one hook becomes the
/// payload observed by the next hook.
#[derive(Clone, Copy, Debug)]
pub struct HookInvocation<'a> {
    event: HookEvent,
    payload: &'a Value,
}

impl HookInvocation<'_> {
    #[must_use]
    pub const fn event(&self) -> HookEvent {
        self.event
    }

    #[must_use]
    pub const fn payload(&self) -> &Value {
        self.payload
    }
}

/// A hook's requested effect on the dispatch pipeline.
#[derive(Clone, Debug, PartialEq)]
pub enum HookDirective {
    Continue,
    Replace(Value),
    Block { message: String },
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
            code: code.into(),
            message: message.into(),
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
    payload: Value,
    status: HookDispatchStatus,
    failures: Vec<HookFailure>,
}

impl HookDispatchResult {
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
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
    #[error("hook `{id}` is already registered for {event:?}")]
    Duplicate { event: HookEvent, id: String },
}

struct RegisteredHook {
    registration: HookRegistration,
    handler: Arc<dyn HookHandler>,
}

/// Deterministic request/response hook dispatcher.
///
/// Hooks execute serially by `(priority, id)`. This makes the observable
/// pipeline independent of extension discovery order and asynchronous runtime
/// scheduling.
#[derive(Default)]
pub struct HookDispatcher {
    hooks: BTreeMap<HookEvent, Vec<RegisteredHook>>,
}

impl HookDispatcher {
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
        event_hooks.push(RegisteredHook {
            registration,
            handler,
        });
        event_hooks.sort_by(|left, right| {
            left.registration
                .priority()
                .cmp(&right.registration.priority())
                .then_with(|| left.registration.id().cmp(right.registration.id()))
        });
        Ok(())
    }

    /// Removes and reports whether an exact `(event, id)` registration existed.
    pub fn unregister(&mut self, event: HookEvent, id: &str) -> bool {
        let Some(event_hooks) = self.hooks.get_mut(&event) else {
            return false;
        };
        let original_len = event_hooks.len();
        event_hooks.retain(|registered| registered.registration.id() != id);
        let removed = event_hooks.len() != original_len;
        if event_hooks.is_empty() {
            self.hooks.remove(&event);
        }
        removed
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

    /// Runs the event pipeline serially and applies its failure policies.
    pub async fn dispatch(&self, event: HookEvent, mut payload: Value) -> HookDispatchResult {
        let mut failures = Vec::new();
        let Some(event_hooks) = self.hooks.get(&event) else {
            return HookDispatchResult {
                payload,
                status: HookDispatchStatus::Completed,
                failures,
            };
        };

        for registered in event_hooks {
            let invocation = HookInvocation {
                event,
                payload: &payload,
            };
            let invoked = tokio::time::timeout(
                registered.registration.timeout(),
                AssertUnwindSafe(registered.handler.invoke(invocation)).catch_unwind(),
            )
            .await
            .map_or_else(
                |_| {
                    Err(HookError::new(
                        "timeout",
                        "hook invocation exceeded its configured deadline",
                    ))
                },
                |result| {
                    result.unwrap_or_else(|_| {
                        Err(HookError::new("panic", "hook implementation panicked"))
                    })
                },
            );
            match invoked {
                Ok(HookDirective::Continue) => {}
                Ok(HookDirective::Replace(replacement)) => payload = replacement,
                Ok(HookDirective::Block { message }) => {
                    return HookDispatchResult {
                        payload,
                        status: HookDispatchStatus::Blocked {
                            hook_id: registered.registration.id.clone(),
                            message,
                        },
                        failures,
                    };
                }
                Err(error) => {
                    let policy = registered.registration.failure_policy();
                    failures.push(HookFailure {
                        hook_id: registered.registration.id.clone(),
                        policy,
                        error,
                    });
                    if policy == HookFailurePolicy::FailClosed {
                        return HookDispatchResult {
                            payload,
                            status: HookDispatchStatus::FailedClosed {
                                hook_id: registered.registration.id.clone(),
                            },
                            failures,
                        };
                    }
                }
            }
        }

        HookDispatchResult {
            payload,
            status: HookDispatchStatus::Completed,
            failures,
        }
    }
}

fn validate_id(id: &str) -> Result<(), HookRegistrationError> {
    if id.is_empty() || id.chars().any(char::is_control) {
        Err(HookRegistrationError::InvalidId)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;

    struct Record {
        calls: Arc<Mutex<Vec<String>>>,
        directive: Result<HookDirective, HookError>,
    }

    struct NeverReturns;

    #[async_trait]
    impl HookHandler for NeverReturns {
        async fn invoke(
            &self,
            _invocation: HookInvocation<'_>,
        ) -> Result<HookDirective, HookError> {
            std::future::pending().await
        }
    }

    #[async_trait]
    impl HookHandler for Record {
        async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
            self.calls
                .lock()
                .expect("test calls lock")
                .push(invocation.payload().to_string());
            self.directive.clone()
        }
    }

    fn record(
        calls: &Arc<Mutex<Vec<String>>>,
        directive: Result<HookDirective, HookError>,
    ) -> Record {
        Record {
            calls: Arc::clone(calls),
            directive,
        }
    }

    #[tokio::test]
    async fn per_registration_timeout_uses_the_declared_failure_policy() {
        for (policy, completed) in [
            (HookFailurePolicy::FailOpen, true),
            (HookFailurePolicy::FailClosed, false),
        ] {
            let mut dispatcher = HookDispatcher::new();
            dispatcher
                .register(
                    HookRegistration::new("timeout", HookEvent::PreTool)
                        .with_timeout(Duration::from_millis(1))
                        .with_failure_policy(policy),
                    NeverReturns,
                )
                .expect("timeout hook");
            let result = dispatcher.dispatch(HookEvent::PreTool, Value::Null).await;
            assert_eq!(result.completed(), completed);
            assert_eq!(result.failures().len(), 1);
            assert_eq!(result.failures()[0].error().code(), "timeout");
        }
    }

    #[tokio::test]
    async fn ordering_is_priority_then_id_and_replacements_form_a_pipeline() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        dispatcher
            .register(
                HookRegistration::new("z-last", HookEvent::PreTool).with_priority(10),
                record(&calls, Ok(HookDirective::Continue)),
            )
            .expect("valid hook");
        dispatcher
            .register(
                HookRegistration::new("b-second", HookEvent::PreTool),
                record(&calls, Ok(HookDirective::Replace(json!({ "step": 2 })))),
            )
            .expect("valid hook");
        dispatcher
            .register(
                HookRegistration::new("a-first", HookEvent::PreTool),
                record(&calls, Ok(HookDirective::Replace(json!({ "step": 1 })))),
            )
            .expect("valid hook");

        let order: Vec<_> = dispatcher
            .registrations(HookEvent::PreTool)
            .map(HookRegistration::id)
            .collect();
        assert_eq!(order, ["a-first", "b-second", "z-last"]);

        let result = dispatcher
            .dispatch(HookEvent::PreTool, json!({ "step": 0 }))
            .await;
        assert!(result.completed());
        assert_eq!(result.payload(), &json!({ "step": 2 }));
        assert_eq!(
            *calls.lock().expect("test calls lock"),
            [r#"{"step":0}"#, r#"{"step":1}"#, r#"{"step":2}"#]
        );
    }

    #[tokio::test]
    async fn fail_open_records_failure_and_continues_with_unchanged_payload() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        dispatcher
            .register(
                HookRegistration::new("broken", HookEvent::PostTool)
                    .with_failure_policy(HookFailurePolicy::FailOpen),
                record(&calls, Err(HookError::new("timeout", "too slow"))),
            )
            .expect("valid hook");
        dispatcher
            .register(
                HookRegistration::new("recover", HookEvent::PostTool).with_priority(1),
                record(&calls, Ok(HookDirective::Replace(json!("recovered")))),
            )
            .expect("valid hook");

        let result = dispatcher
            .dispatch(HookEvent::PostTool, json!("original"))
            .await;
        assert!(result.completed());
        assert_eq!(result.payload(), &json!("recovered"));
        assert_eq!(result.failures().len(), 1);
        assert_eq!(result.failures()[0].hook_id(), "broken");
        assert_eq!(result.failures()[0].policy(), HookFailurePolicy::FailOpen);
        assert_eq!(result.failures()[0].error().code(), "timeout");
        assert_eq!(calls.lock().expect("test calls lock").len(), 2);
    }

    #[tokio::test]
    async fn fail_closed_records_failure_and_stops_later_hooks() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        dispatcher
            .register(
                HookRegistration::new("closed", HookEvent::PermissionCheck)
                    .with_failure_policy(HookFailurePolicy::FailClosed),
                record(&calls, Err(HookError::new("offline", "policy unavailable"))),
            )
            .expect("valid hook");
        dispatcher
            .register(
                HookRegistration::new("never", HookEvent::PermissionCheck).with_priority(1),
                record(&calls, Ok(HookDirective::Continue)),
            )
            .expect("valid hook");

        let result = dispatcher
            .dispatch(HookEvent::PermissionCheck, json!(42))
            .await;
        assert_eq!(
            result.status(),
            &HookDispatchStatus::FailedClosed {
                hook_id: "closed".to_owned()
            }
        );
        assert_eq!(result.payload(), &json!(42));
        assert_eq!(result.failures().len(), 1);
        assert_eq!(result.failures()[0].policy(), HookFailurePolicy::FailClosed);
        assert_eq!(calls.lock().expect("test calls lock").len(), 1);
    }

    #[tokio::test]
    async fn explicit_block_stops_regardless_of_failure_policy() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        dispatcher
            .register(
                HookRegistration::new("deny", HookEvent::UserPromptSubmit)
                    .with_failure_policy(HookFailurePolicy::FailOpen),
                record(
                    &calls,
                    Ok(HookDirective::Block {
                        message: "org policy".to_owned(),
                    }),
                ),
            )
            .expect("valid hook");
        dispatcher
            .register(
                HookRegistration::new("never", HookEvent::UserPromptSubmit).with_priority(1),
                record(&calls, Ok(HookDirective::Continue)),
            )
            .expect("valid hook");

        let result = dispatcher
            .dispatch(HookEvent::UserPromptSubmit, json!({ "prompt": "secret" }))
            .await;
        assert_eq!(
            result.status(),
            &HookDispatchStatus::Blocked {
                hook_id: "deny".to_owned(),
                message: "org policy".to_owned()
            }
        );
        assert!(result.failures().is_empty());
        assert_eq!(calls.lock().expect("test calls lock").len(), 1);
    }

    #[tokio::test]
    async fn registrations_are_event_scoped_and_empty_dispatch_is_identity() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        dispatcher
            .register(
                HookRegistration::new("shared-id", HookEvent::SessionStart),
                record(&calls, Ok(HookDirective::Continue)),
            )
            .expect("valid hook");
        dispatcher
            .register(
                HookRegistration::new("shared-id", HookEvent::SessionEnd),
                record(&calls, Ok(HookDirective::Continue)),
            )
            .expect("same ID is valid for a different event");
        assert_eq!(
            dispatcher.register(
                HookRegistration::new("shared-id", HookEvent::SessionStart),
                record(&calls, Ok(HookDirective::Continue))
            ),
            Err(HookRegistrationError::Duplicate {
                event: HookEvent::SessionStart,
                id: "shared-id".to_owned()
            })
        );

        let payload = json!({ "unchanged": true });
        let result = dispatcher
            .dispatch(HookEvent::TurnEnd, payload.clone())
            .await;
        assert!(result.completed());
        assert_eq!(result.payload(), &payload);
        assert!(dispatcher.unregister(HookEvent::SessionStart, "shared-id"));
        assert!(!dispatcher.unregister(HookEvent::SessionStart, "shared-id"));
        assert_eq!(dispatcher.registrations(HookEvent::SessionEnd).len(), 1);
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        assert_eq!(
            dispatcher.register(
                HookRegistration::new("bad\nid", HookEvent::PreCompact),
                record(&calls, Ok(HookDirective::Continue))
            ),
            Err(HookRegistrationError::InvalidId)
        );
    }
}
