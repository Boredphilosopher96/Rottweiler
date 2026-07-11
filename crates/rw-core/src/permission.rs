use std::{collections::BTreeSet, fmt, sync::RwLock};

use async_trait::async_trait;
use rw_types::{ApprovalDecision, ToolCapability, UnifiedDiff, config::PermissionDecision};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One tool invocation presented to the permission chokepoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PermissionRequest {
    /// Stable model-assigned tool-call id.
    pub id: String,
    /// Registered tool name.
    pub tool_name: String,
    /// Provider-neutral parsed arguments.
    pub arguments: Value,
    /// Declared effects used by policy and the active client.
    pub capabilities: Vec<ToolCapability>,
    /// Exact visual proposal, when this invocation supports a diff-bound ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_diff: Option<UnifiedDiff>,
}

/// Result of the one mandatory permission check before tool execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionOutcome {
    /// The invocation may continue to pre-tool hooks and execution.
    Allowed,
    /// The invocation must become an error tool result without executing.
    Denied,
}

/// Minimal non-interactive permission policy selected by the headless CLI.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeadlessPermissionMode {
    /// Ask for every invocation through the supplied approver.
    Strict,
    /// Allow only empty/no-effect or exclusively read-filesystem manifests.
    AutoSafe,
    /// Allow all manifests. The CLI owns the root/workspace footgun rails.
    Yolo,
}

/// Interactive approval boundary supplied by a session actor or headless policy.
#[async_trait]
pub trait PermissionApprover: Send + Sync {
    /// Returns the active driver's decision for one ask-tier invocation.
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision;
}

/// Minimal M2 permission policy. Pattern rules and mode overlays are additive M5 work.
pub struct PermissionGate {
    policy: PermissionPolicy,
    session_allows: RwLock<BTreeSet<PermissionKey>>,
}

impl fmt::Debug for PermissionGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let remembered = self
            .session_allows
            .read()
            .map_or(0, |entries| entries.len());
        formatter
            .debug_struct("PermissionGate")
            .field("policy", &self.policy)
            .field("remembered_session_allow_count", &remembered)
            .finish()
    }
}

impl PermissionGate {
    /// Creates the single execution chokepoint for a session.
    #[must_use]
    pub fn new(default: PermissionDecision) -> Self {
        Self {
            policy: PermissionPolicy::Uniform(default),
            session_allows: RwLock::new(BTreeSet::new()),
        }
    }

    /// Creates the minimal policy used by non-interactive CLI modes.
    #[must_use]
    pub fn for_headless_mode(mode: HeadlessPermissionMode) -> Self {
        Self {
            policy: PermissionPolicy::Headless(mode),
            session_allows: RwLock::new(BTreeSet::new()),
        }
    }

    /// Applies static policy, remembered session approvals, and ask-tier input.
    pub async fn authorize(
        &self,
        request: PermissionRequest,
        approver: &dyn PermissionApprover,
    ) -> PermissionOutcome {
        self.authorize_with_override(request, approver, None).await
    }

    /// Applies an allow/deny supplement from the `permission_check` hook for
    /// ask-tier requests. Static allow/deny policy always remains authoritative.
    pub async fn authorize_with_override(
        &self,
        request: PermissionRequest,
        approver: &dyn PermissionApprover,
        ask_override: Option<PermissionOutcome>,
    ) -> PermissionOutcome {
        let key = PermissionKey::from(&request);
        match self.decision_for(&request) {
            PermissionDecision::Allow => PermissionOutcome::Allowed,
            PermissionDecision::Deny => PermissionOutcome::Denied,
            PermissionDecision::Ask => {
                if let Some(outcome) = ask_override {
                    return outcome;
                }
                if self
                    .session_allows
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(&key)
                {
                    return PermissionOutcome::Allowed;
                }
                match approver.decide(request).await {
                    ApprovalDecision::AllowOnce => PermissionOutcome::Allowed,
                    ApprovalDecision::AllowSession => {
                        self.session_allows
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(key);
                        PermissionOutcome::Allowed
                    }
                    ApprovalDecision::Deny => PermissionOutcome::Denied,
                }
            }
        }
    }

    fn decision_for(&self, request: &PermissionRequest) -> PermissionDecision {
        match self.policy {
            PermissionPolicy::Uniform(decision) => decision,
            PermissionPolicy::Headless(HeadlessPermissionMode::Strict) => PermissionDecision::Ask,
            PermissionPolicy::Headless(HeadlessPermissionMode::Yolo) => PermissionDecision::Allow,
            PermissionPolicy::Headless(HeadlessPermissionMode::AutoSafe) => {
                if request
                    .capabilities
                    .iter()
                    .all(|capability| matches!(capability, ToolCapability::ReadFilesystem))
                {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum PermissionPolicy {
    Uniform(PermissionDecision),
    Headless(HeadlessPermissionMode),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PermissionKey {
    tool_name: String,
    arguments: String,
    capabilities: Vec<String>,
}

impl From<&PermissionRequest> for PermissionKey {
    fn from(request: &PermissionRequest) -> Self {
        let mut capabilities = request
            .capabilities
            .iter()
            .map(|capability| format!("{capability:?}"))
            .collect::<Vec<_>>();
        capabilities.sort();
        capabilities.dedup();
        let arguments = serde_json::to_string(&request.arguments)
            .unwrap_or_else(|_| "unserializable-arguments".to_owned());
        Self {
            tool_name: request.tool_name.clone(),
            arguments,
            capabilities,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Approver {
        decision: ApprovalDecision,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl PermissionApprover for Approver {
        async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.decision.clone()
        }
    }

    fn request(id: &str) -> PermissionRequest {
        PermissionRequest {
            id: id.to_owned(),
            tool_name: "write".to_owned(),
            arguments: serde_json::json!({"path": "fixture"}),
            capabilities: vec![ToolCapability::WriteFilesystem],
            approval_diff: None,
        }
    }

    #[tokio::test]
    async fn static_allow_and_deny_never_ask() {
        for (decision, expected) in [
            (PermissionDecision::Allow, PermissionOutcome::Allowed),
            (PermissionDecision::Deny, PermissionOutcome::Denied),
        ] {
            let approver = Approver {
                decision: ApprovalDecision::Deny,
                calls: AtomicUsize::new(0),
            };
            let outcome = PermissionGate::new(decision)
                .authorize(request("call-1"), &approver)
                .await;
            assert_eq!(outcome, expected);
            assert_eq!(approver.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn ask_honors_once_deny_and_session_memory() {
        let once = Approver {
            decision: ApprovalDecision::AllowOnce,
            calls: AtomicUsize::new(0),
        };
        let gate = PermissionGate::new(PermissionDecision::Ask);
        assert_eq!(
            gate.authorize(request("once"), &once).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(request("twice"), &once).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(once.calls.load(Ordering::SeqCst), 2);

        let deny = Approver {
            decision: ApprovalDecision::Deny,
            calls: AtomicUsize::new(0),
        };
        assert_eq!(
            gate.authorize(request("deny"), &deny).await,
            PermissionOutcome::Denied
        );

        let session = Approver {
            decision: ApprovalDecision::AllowSession,
            calls: AtomicUsize::new(0),
        };
        assert_eq!(
            gate.authorize(request("remember"), &session).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(request("remembered"), &session).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(session.calls.load(Ordering::SeqCst), 1);

        let mut different_arguments = request("different");
        different_arguments.arguments = serde_json::json!({"path": "another-fixture"});
        assert_eq!(
            gate.authorize(different_arguments, &session).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(session.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn headless_auto_safe_allows_only_complete_read_only_manifests() {
        let approver = Approver {
            decision: ApprovalDecision::AllowOnce,
            calls: AtomicUsize::new(0),
        };
        let gate = PermissionGate::for_headless_mode(HeadlessPermissionMode::AutoSafe);
        let mut read = request("read");
        read.capabilities = vec![ToolCapability::ReadFilesystem];
        assert_eq!(
            gate.authorize(read, &approver).await,
            PermissionOutcome::Allowed
        );
        let mut mixed = request("mixed");
        mixed.capabilities = vec![ToolCapability::ReadFilesystem, ToolCapability::Network];
        assert_eq!(
            gate.authorize(mixed, &approver).await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn current_hook_deny_overrides_an_exact_remembered_session_allow() {
        let approver = Approver {
            decision: ApprovalDecision::AllowSession,
            calls: AtomicUsize::new(0),
        };
        let gate = PermissionGate::new(PermissionDecision::Ask);
        assert_eq!(
            gate.authorize(request("remember"), &approver).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize_with_override(
                request("same-invocation"),
                &approver,
                Some(PermissionOutcome::Denied),
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.calls.load(Ordering::SeqCst), 1);
    }
}
