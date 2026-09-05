use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use async_trait::async_trait;
use rw_tools::{
    BashSandboxMode, CommandSafety, CommandSafetyClassifier, ToolBehavior, ToolInvocationSemantics,
};
use rw_types::{
    ApprovalDecision, PermissionModeDescriptor, SessionMode, ToolCapability, UnifiedDiff,
    config::{PermissionConfig, PermissionDecision, PermissionRule},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One tool invocation presented to the permission chokepoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PermissionRequest {
    pub id: String,
    pub invocation_id: rw_types::ToolInvocationId,
    pub tool_name: String,
    pub arguments: Value,
    pub capabilities: Vec<ToolCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_diff: Option<UnifiedDiff>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionOutcome {
    Allowed,
    Denied,
    RememberedApprovalUnavailable,
}

#[async_trait]
pub trait PermissionApprover: Send + Sync {
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision;
}

/// Introspection returned by `/permissions` without exposing filesystem internals.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PermissionSnapshot {
    pub default: PermissionDecision,
    pub runtime_mode: Option<PermissionModeDescriptor>,
    pub rules: Vec<PermissionRule>,
    pub session_rules: Vec<PermissionRule>,
    pub session_approvals: usize,
    pub project_approvals: usize,
}

/// Opaque, non-secret description of one remembered exact approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PermissionApprovalSummary {
    pub id: String,
    pub tool_name: String,
    pub canonical_summary: String,
}

/// Remembered approvals grouped by their revocation scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PermissionApprovalSnapshot {
    pub session: Vec<PermissionApprovalSummary>,
    pub project: Vec<PermissionApprovalSummary>,
}

/// Counts returned after clearing session-scoped permission state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClearedSessionPermissions {
    pub rules: usize,
    pub approvals: usize,
}

/// Result of atomically switching the permission workspace generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionGenerationUpdate {
    pub generation: u64,
    pub invalidated_session_approvals: usize,
    pub invalidated_project_approvals: usize,
}

#[derive(Default)]
struct PermissionMemory {
    workspace_roots: Vec<PathBuf>,
    trusted_read_roots: Vec<PathBuf>,
    workspace_namespace: Vec<String>,
    generation: u64,
    session_allows: BTreeSet<RememberedApproval>,
}

/// Single mandatory permission chokepoint with mode overlays, pattern rules,
/// and exact invocation approvals remembered at session or project scope.
pub struct PermissionGate {
    policy: PermissionPolicy,
    runtime_mode: Arc<RwLock<Option<PermissionModeDescriptor>>>,
    restrictive_rules: Option<Vec<PermissionRule>>,
    memory: RwLock<PermissionMemory>,
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    project_store: Option<Arc<ProjectApprovalStore>>,
    command_safety: Arc<CommandSafetyClassifier>,
}

impl fmt::Debug for PermissionGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionGate")
            .field("policy", &self.policy)
            .field("snapshot", &self.snapshot())
            .field("project_persistence", &self.project_store.is_some())
            .finish_non_exhaustive()
    }
}

impl PermissionGate {
    #[must_use]
    pub fn new(default: PermissionDecision) -> Self {
        Self::from_config(PermissionConfig {
            default,
            rules: Vec::new(),
        })
    }

    #[must_use]
    pub fn from_config(config: PermissionConfig) -> Self {
        Self {
            policy: PermissionPolicy::Configured(config),
            runtime_mode: Arc::new(RwLock::new(None)),
            restrictive_rules: None,
            memory: RwLock::new(PermissionMemory::default()),
            session_rules: Arc::new(RwLock::new(Vec::new())),
            project_store: None,
            command_safety: Arc::new(CommandSafetyClassifier::default()),
        }
    }

    /// Enables durable exact-invocation approvals. Unsafe or malformed files
    /// fail closed by loading no approvals; writes remain atomic and private.
    #[must_use]
    pub fn with_project_approval_file(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.project_store = Some(shared_project_store(&path));
        self
    }

    #[must_use]
    pub fn for_headless_mode(mode: PermissionModeDescriptor) -> Self {
        Self {
            policy: PermissionPolicy::Headless(mode),
            runtime_mode: Arc::new(RwLock::new(None)),
            restrictive_rules: None,
            memory: RwLock::new(PermissionMemory::default()),
            session_rules: Arc::new(RwLock::new(Vec::new())),
            project_store: None,
            command_safety: Arc::new(CommandSafetyClassifier::default()),
        }
    }

    /// Installs the same immutable user-scoped classifier used by bash execution.
    #[must_use]
    pub fn with_command_safety(mut self, safety: Arc<CommandSafetyClassifier>) -> Self {
        self.command_safety = safety;
        self
    }

    /// Lets explicitly trusted workspace roots use built-in read-only tools.
    /// Paths are still resolved against the active workspace generation, and
    /// writes, execution, network, and explicit deny rules remain unaffected.
    #[must_use]
    pub fn with_trusted_read_roots(
        self,
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Self {
        let trusted = canonical_workspace_roots(roots);
        let mut memory = lock_write(&self.memory);
        memory.trusted_read_roots = trusted
            .into_iter()
            .filter(|trusted| memory.workspace_roots.contains(trusted))
            .collect();
        drop(memory);
        self
    }

    #[must_use]
    pub fn snapshot(&self) -> PermissionSnapshot {
        let runtime_mode = *lock_read(&self.runtime_mode);
        let (base_default, rules) = match &self.policy {
            PermissionPolicy::Configured(config) => (config.default, config.rules.clone()),
            PermissionPolicy::Headless(PermissionModeDescriptor::Strict) => {
                (PermissionDecision::Ask, Vec::new())
            }
            PermissionPolicy::Headless(PermissionModeDescriptor::AutoSafe) => {
                (PermissionDecision::Deny, Vec::new())
            }
            PermissionPolicy::Headless(PermissionModeDescriptor::Yolo) => {
                (PermissionDecision::Allow, Vec::new())
            }
        };
        let default = runtime_mode.map_or(base_default, permission_mode_default);
        let session_rules = lock_read(&self.session_rules).clone();
        let memory = lock_read(&self.memory);
        let project_approvals = self
            .project_store
            .as_ref()
            .and_then(|store| store.refresh().ok())
            .map_or(0, |approvals| {
                approvals
                    .iter()
                    .filter(|approval| {
                        approval.key.workspace_namespace == memory.workspace_namespace
                    })
                    .count()
            });
        PermissionSnapshot {
            default,
            runtime_mode,
            rules,
            session_rules,
            session_approvals: memory.session_allows.len(),
            project_approvals,
        }
    }

    /// Applies a session-local interactive permission policy override.
    ///
    /// Explicit headless policies (including the remote strict guard) are not
    /// switchable from a client command. The override never weakens mode
    /// overlays, explicit deny rules, sandbox validation, or malformed-input
    /// rejection.
    ///
    /// # Errors
    ///
    /// Returns an error for launch-fixed policies or the root-at-`/` yolo
    /// footgun.
    pub fn set_runtime_mode(&self, mode: Option<PermissionModeDescriptor>) -> Result<(), String> {
        if matches!(self.policy, PermissionPolicy::Headless(_)) {
            return Err(
                "permission mode is fixed by the process launch policy and cannot be changed in this session"
                    .to_owned(),
            );
        }
        if mode == Some(PermissionModeDescriptor::Yolo) {
            let memory = lock_read(&self.memory);
            if root_yolo_footgun(
                rustix::process::geteuid().is_root(),
                &memory.workspace_roots,
            ) {
                return Err("yolo mode is refused for root while a workspace root is /".to_owned());
            }
        }
        *lock_write(&self.runtime_mode) = mode;
        Ok(())
    }

    /// Binds remembered approvals to the complete ordered root identity.
    #[must_use]
    pub fn with_workspace_roots(self, roots: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        let roots = canonical_workspace_roots(roots);
        {
            let mut memory = lock_write(&self.memory);
            memory.workspace_namespace = workspace_namespace(&roots);
            memory.workspace_roots = roots;
            memory.generation = 1;
        }
        self
    }

    /// Atomically switches the complete ordered root identity and invalidates
    /// approvals granted under the previous generation. An unchanged root set
    /// is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error when clearing the private project approval file fails;
    /// the in-memory generation is left unchanged in that case.
    pub fn replace_workspace_roots(
        &self,
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Result<PermissionGenerationUpdate, std::io::Error> {
        let roots = canonical_workspace_roots(roots);
        if self.yolo_active() && root_yolo_footgun(rustix::process::geteuid().is_root(), &roots) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "yolo mode is refused for root while a workspace root is /",
            ));
        }
        let namespace = workspace_namespace(&roots);
        let mut memory = lock_write(&self.memory);
        if memory.workspace_namespace == namespace {
            return Ok(PermissionGenerationUpdate {
                generation: memory.generation,
                invalidated_session_approvals: 0,
                invalidated_project_approvals: 0,
            });
        }
        let invalidated_project_approvals = if let Some(store) = &self.project_store {
            store.clear_all()?
        } else {
            0
        };
        let update = PermissionGenerationUpdate {
            generation: memory.generation.saturating_add(1),
            invalidated_session_approvals: memory.session_allows.len(),
            invalidated_project_approvals,
        };
        memory.workspace_namespace = namespace;
        memory
            .trusted_read_roots
            .retain(|trusted| roots.contains(trusted));
        memory.workspace_roots = roots;
        memory.generation = update.generation;
        memory.session_allows.clear();
        Ok(update)
    }

    /// Prepares a replacement gate for an atomic live-root generation swap.
    /// Policy overlays and session rules remain shared with the parent gate so
    /// delegated sessions observe live mode changes. Remembered invocation
    /// approvals are omitted because their workspace namespace changed. Project
    /// approvals remain untouched in the shared ledger and cannot match the
    /// replacement's new namespace.
    /// The current gate's policy, rules, namespace, generation, and session
    /// approvals are never mutated.
    ///
    /// # Errors
    ///
    /// This operation performs no persistence, but retains a fallible return
    /// type so controllers can use one prepare/swap contract.
    pub fn fork_for_workspace_roots(
        &self,
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Result<Self, std::io::Error> {
        let roots = canonical_workspace_roots(roots);
        if self.yolo_active() && root_yolo_footgun(rustix::process::geteuid().is_root(), &roots) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "yolo mode is refused for root while a workspace root is /",
            ));
        }
        let generation = lock_read(&self.memory).generation.saturating_add(1);
        Ok(Self {
            policy: self.policy.clone(),
            runtime_mode: Arc::clone(&self.runtime_mode),
            restrictive_rules: self.restrictive_rules.clone(),
            memory: RwLock::new(PermissionMemory {
                workspace_namespace: workspace_namespace(&roots),
                trusted_read_roots: lock_read(&self.memory)
                    .trusted_read_roots
                    .iter()
                    .filter(|trusted| roots.contains(trusted))
                    .cloned()
                    .collect(),
                workspace_roots: roots,
                generation,
                session_allows: BTreeSet::new(),
            }),
            session_rules: Arc::clone(&self.session_rules),
            project_store: self.project_store.clone(),
            command_safety: Arc::clone(&self.command_safety),
        })
    }

    /// Clones this gate for one turn and adds a fail-closed invocation
    /// allowlist. Base policy still applies after an invocation matches one of
    /// these patterns, so this can only remove authority.
    ///
    /// # Errors
    ///
    /// Returns an error if any pattern is not valid `tool(glob)` syntax.
    pub fn restricted_to_patterns(&self, patterns: &[String]) -> Result<Self, String> {
        let restrictive_rules = patterns
            .iter()
            .map(|pattern| {
                validate_rule(pattern)?;
                Ok(PermissionRule {
                    pattern: pattern.clone(),
                    action: PermissionDecision::Allow,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let memory = lock_read(&self.memory);
        Ok(Self {
            policy: self.policy.clone(),
            runtime_mode: Arc::clone(&self.runtime_mode),
            restrictive_rules: Some(restrictive_rules),
            memory: RwLock::new(PermissionMemory {
                workspace_roots: memory.workspace_roots.clone(),
                trusted_read_roots: memory.trusted_read_roots.clone(),
                workspace_namespace: memory.workspace_namespace.clone(),
                generation: memory.generation,
                session_allows: memory.session_allows.clone(),
            }),
            session_rules: Arc::clone(&self.session_rules),
            project_store: self.project_store.clone(),
            command_safety: Arc::clone(&self.command_safety),
        })
    }

    pub(crate) fn registered_execution_identity(
        request: &PermissionRequest,
        semantics: &ToolInvocationSemantics,
    ) -> String {
        Self::execution_identity_with_behavior(request, semantics.behavior)
    }

    fn execution_identity_with_behavior(
        request: &PermissionRequest,
        behavior: ToolBehavior,
    ) -> String {
        fingerprint(
            b"rottweiler-permission-execution-identity-v1\0",
            canonical_key_arguments_for(request, behavior).as_bytes(),
        )
    }

    /// Adds or replaces one session-scoped rule. Session rules disappear when
    /// the actor exits and never modify project configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the rule does not use `tool(glob)` syntax or its
    /// glob cannot be compiled.
    pub fn add_session_rule(&self, rule: PermissionRule) -> Result<(), String> {
        validate_rule(&rule.pattern)?;
        let mut rules = lock_write(&self.session_rules);
        rules.retain(|existing| existing.pattern != rule.pattern);
        rules.push(rule);
        Ok(())
    }

    /// Removes a session-scoped rule with the exact normalized pattern.
    pub fn remove_session_rule(&self, pattern: &str) -> bool {
        let mut rules = lock_write(&self.session_rules);
        let before = rules.len();
        rules.retain(|rule| rule.pattern != pattern);
        rules.len() != before
    }

    /// Clears all session-scoped rules and returns the number removed.
    pub fn clear_session_rules(&self) -> usize {
        let mut rules = lock_write(&self.session_rules);
        let removed = rules.len();
        rules.clear();
        removed
    }

    /// Clears session-scoped rules and remembered `AllowSession` decisions.
    pub fn clear_session_permissions(&self) -> ClearedSessionPermissions {
        let rules = self.clear_session_rules();
        let mut memory = lock_write(&self.memory);
        let approvals = memory.session_allows.len();
        memory.session_allows.clear();
        ClearedSessionPermissions { rules, approvals }
    }

    /// Returns opaque approval ids and summaries containing metadata only,
    /// never canonical argument values or fingerprints.
    #[must_use]
    pub fn approval_snapshot(&self) -> PermissionApprovalSnapshot {
        let memory = lock_read(&self.memory);
        let project = self
            .project_store
            .as_ref()
            .and_then(|store| store.refresh().ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|approval| approval.key.workspace_namespace == memory.workspace_namespace)
            .collect::<BTreeSet<_>>();
        PermissionApprovalSnapshot {
            session: memory
                .session_allows
                .iter()
                .map(RememberedApproval::summary)
                .collect(),
            project: project.iter().map(RememberedApproval::summary).collect(),
        }
    }

    /// Revokes one opaque session approval id, or all session approvals when
    /// `id` is `None`. Returns the number removed.
    pub fn revoke_session_approvals(&self, id: Option<&str>) -> usize {
        let mut memory = lock_write(&self.memory);
        revoke_approvals(&mut memory.session_allows, id)
    }

    /// Revokes one opaque project approval id, or all project approvals when
    /// `id` is `None`, and atomically updates private persistence.
    ///
    /// # Errors
    ///
    /// Returns an error if the updated project approval set cannot be
    /// persisted. In-memory approvals remain unchanged on failure.
    pub fn revoke_project_approvals(&self, id: Option<&str>) -> Result<usize, std::io::Error> {
        self.project_store
            .as_ref()
            .map_or(Ok(0), |store| store.revoke(id))
    }

    pub async fn authorize(
        &self,
        request: PermissionRequest,
        approver: &dyn PermissionApprover,
    ) -> PermissionOutcome {
        self.authorize_in_mode(request, approver, None, SessionMode::Execute)
            .await
    }

    pub async fn authorize_with_override(
        &self,
        request: PermissionRequest,
        approver: &dyn PermissionApprover,
        ask_override: Option<PermissionOutcome>,
    ) -> PermissionOutcome {
        self.authorize_in_mode(request, approver, ask_override, SessionMode::Execute)
            .await
    }

    fn accept_for_generation(
        &self,
        generation: u64,
        approval: Option<RememberedApproval>,
    ) -> PermissionOutcome {
        let mut memory = lock_write(&self.memory);
        if memory.generation != generation {
            return PermissionOutcome::Denied;
        }
        // The displayed invocation was approved. Failure to allocate the
        // opaque id used only for remembering future invocations must not
        // retroactively deny this one; degrade to allow-once instead.
        if let Some(approval) = approval {
            replace_approval(&mut memory.session_allows, approval);
        }
        PermissionOutcome::Allowed
    }

    /// Applies the mode overlay before configured policy. Discuss and Plan can
    /// never authorize an invocation with mutating or ambient capabilities,
    /// even under yolo or an auto-approval hook.
    pub async fn authorize_in_mode(
        &self,
        request: PermissionRequest,
        approver: &dyn PermissionApprover,
        ask_override: Option<PermissionOutcome>,
        mode: SessionMode,
    ) -> PermissionOutcome {
        self.authorize_in_mode_with_semantics(request, approver, ask_override, mode, None)
            .await
    }

    /// Authorizes an invocation using semantics resolved by the tool registry.
    /// The registered path is the production boundary; callers that do not
    /// execute local tools may continue using [`Self::authorize_in_mode`].
    pub(crate) async fn authorize_registered_in_mode(
        &self,
        request: PermissionRequest,
        semantics: &ToolInvocationSemantics,
        approver: &dyn PermissionApprover,
        ask_override: Option<PermissionOutcome>,
        mode: SessionMode,
    ) -> PermissionOutcome {
        self.authorize_in_mode_with_semantics(
            request,
            approver,
            ask_override,
            mode,
            Some(semantics),
        )
        .await
    }

    async fn authorize_in_mode_with_semantics(
        &self,
        request: PermissionRequest,
        approver: &dyn PermissionApprover,
        ask_override: Option<PermissionOutcome>,
        mode: SessionMode,
        semantics: Option<&ToolInvocationSemantics>,
    ) -> PermissionOutcome {
        let behavior = semantics.map_or(ToolBehavior::Standard, |semantics| semantics.behavior);
        if request.arguments.get("network_domains").is_some()
            && normalize_network_domains(&request.arguments["network_domains"]).is_none()
        {
            return PermissionOutcome::Denied;
        }
        if behavior == ToolBehavior::Shell && bash_sandbox_mode(&request).is_none() {
            return PermissionOutcome::Denied;
        }
        if behavior == ToolBehavior::WebFetch
            && request
                .arguments
                .get("url")
                .and_then(Value::as_str)
                .and_then(canonical_webfetch_origin)
                .is_none()
        {
            return PermissionOutcome::Denied;
        }
        if behavior == ToolBehavior::PlanSubmission && mode != SessionMode::Plan {
            return PermissionOutcome::Denied;
        }
        if mode != SessionMode::Execute
            && !(is_read_only(&request, behavior) || is_builtin_read_only_bash(&request, behavior))
        {
            return PermissionOutcome::Denied;
        }
        if ask_override == Some(PermissionOutcome::Denied) {
            return PermissionOutcome::Denied;
        }
        match self.decision_for(&request, semantics, behavior) {
            PermissionDecision::Allow => PermissionOutcome::Allowed,
            PermissionDecision::Deny => PermissionOutcome::Denied,
            PermissionDecision::Ask => {
                if let Some(outcome) = ask_override {
                    return outcome;
                }
                let rememberable = rememberable_request(&request, behavior);
                let (key, generation, remembered) = {
                    let memory = lock_read(&self.memory);
                    let key = PermissionKey::from_request_with_behavior(
                        &request,
                        &memory.workspace_namespace,
                        behavior,
                    );
                    let remembered = rememberable
                        && (contains_approval(&memory.session_allows, &key)
                            || self
                                .project_store
                                .as_ref()
                                .is_some_and(|store| store.contains(&key).unwrap_or(false)));
                    (key, memory.generation, remembered)
                };
                if remembered {
                    return PermissionOutcome::Allowed;
                }
                match approver.decide(request).await {
                    ApprovalDecision::AllowOnce => {
                        if lock_read(&self.memory).generation == generation {
                            PermissionOutcome::Allowed
                        } else {
                            PermissionOutcome::Denied
                        }
                    }
                    ApprovalDecision::AllowSession => {
                        if !rememberable {
                            // The user approved the invocation currently on
                            // screen. Some compound or mutable commands cannot
                            // be represented by a safe reusable authority, but
                            // that must not turn the accepted selection into a
                            // failed tool call. Execute this invocation once
                            // and intentionally retain no remembered grant.
                            return self.accept_for_generation(generation, None);
                        }
                        let approval = RememberedApproval::new("session", key);
                        self.accept_for_generation(generation, approval)
                    }
                    ApprovalDecision::AllowProject => {
                        if !rememberable {
                            return self.accept_for_generation(generation, None);
                        }
                        let memory = lock_write(&self.memory);
                        if memory.generation != generation {
                            return PermissionOutcome::Denied;
                        }
                        if let Some(store) = &self.project_store {
                            // Project persistence only widens the approval to
                            // future invocations. If it is unavailable or the
                            // private write fails, the accepted invocation on
                            // screen still executes once.
                            let _ = store.grant(key);
                        }
                        PermissionOutcome::Allowed
                    }
                    ApprovalDecision::Deny => PermissionOutcome::Denied,
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn decision_for(
        &self,
        request: &PermissionRequest,
        semantics: Option<&ToolInvocationSemantics>,
        behavior: ToolBehavior,
    ) -> PermissionDecision {
        if self.yolo_active()
            && root_yolo_footgun(
                rustix::process::geteuid().is_root(),
                &lock_read(&self.memory).workspace_roots,
            )
        {
            return PermissionDecision::Deny;
        }
        if self.restrictive_rules.as_ref().is_some_and(|rules| {
            rule_decision(
                &PermissionConfig {
                    default: PermissionDecision::Deny,
                    rules: rules.clone(),
                },
                request,
                behavior,
            ) != PermissionDecision::Allow
        }) {
            return PermissionDecision::Deny;
        }
        if matches!(
            behavior,
            ToolBehavior::UserInteraction | ToolBehavior::PlanSubmission
        ) && request.capabilities.is_empty()
        {
            return PermissionDecision::Allow;
        }
        let unsandboxed = behavior == ToolBehavior::Shell
            && bash_sandbox_mode(request) == Some(BashSandboxMode::Unsandboxed);
        let safe_listed = self.is_safe_listed_bash(request, behavior);
        let runtime_mode = *lock_read(&self.runtime_mode);
        match &self.policy {
            PermissionPolicy::Configured(config) => {
                let mut effective = config.clone();
                effective
                    .rules
                    .extend(lock_read(&self.session_rules).iter().cloned());
                if let Some(mode) = runtime_mode {
                    effective.default = permission_mode_default(mode);
                }
                let configured = rule_decision(&effective, request, behavior);
                if unsandboxed {
                    let explicit = unsandboxed_rule_decision(&effective, request, behavior);
                    if configured == PermissionDecision::Deny
                        || explicit == Some(PermissionDecision::Deny)
                    {
                        PermissionDecision::Deny
                    } else if explicit == Some(PermissionDecision::Allow)
                        || runtime_mode == Some(PermissionModeDescriptor::Yolo)
                    {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Ask
                    }
                } else if runtime_mode == Some(PermissionModeDescriptor::AutoSafe) {
                    self.auto_safe_decision(
                        request,
                        semantics,
                        behavior,
                        effective.rules,
                        safe_listed,
                    )
                } else if configured == PermissionDecision::Ask
                    && (runtime_mode == Some(PermissionModeDescriptor::Yolo)
                        || !requires_interactive_approval(
                            request,
                            behavior,
                            safe_listed,
                            semantics.is_some(),
                        ))
                {
                    PermissionDecision::Allow
                } else {
                    configured
                }
            }
            PermissionPolicy::Headless(mode) => {
                let default = match mode {
                    PermissionModeDescriptor::Strict => PermissionDecision::Ask,
                    PermissionModeDescriptor::AutoSafe => PermissionDecision::Deny,
                    PermissionModeDescriptor::Yolo => PermissionDecision::Allow,
                };
                let rules = lock_read(&self.session_rules).clone();
                if unsandboxed {
                    let policy = PermissionConfig { default, rules };
                    let configured = rule_decision(&policy, request, behavior);
                    let explicit = unsandboxed_rule_decision(&policy, request, behavior);
                    return match mode {
                        PermissionModeDescriptor::AutoSafe => PermissionDecision::Deny,
                        _ if configured == PermissionDecision::Deny
                            || explicit == Some(PermissionDecision::Deny) =>
                        {
                            PermissionDecision::Deny
                        }
                        PermissionModeDescriptor::Strict
                            if explicit == Some(PermissionDecision::Allow) =>
                        {
                            PermissionDecision::Allow
                        }
                        PermissionModeDescriptor::Strict => PermissionDecision::Ask,
                        PermissionModeDescriptor::Yolo => PermissionDecision::Allow,
                    };
                }
                if *mode == PermissionModeDescriptor::AutoSafe {
                    return self.auto_safe_decision(
                        request,
                        semantics,
                        behavior,
                        rules,
                        safe_listed,
                    );
                }
                if rules.is_empty() {
                    match mode {
                        PermissionModeDescriptor::Strict
                            if !requires_interactive_approval(
                                request,
                                behavior,
                                safe_listed,
                                semantics.is_some(),
                            ) =>
                        {
                            PermissionDecision::Allow
                        }
                        PermissionModeDescriptor::AutoSafe
                            if safe_listed || is_read_only(request, behavior) =>
                        {
                            PermissionDecision::Allow
                        }
                        _ => default,
                    }
                } else {
                    let configured =
                        rule_decision(&PermissionConfig { default, rules }, request, behavior);
                    if configured == PermissionDecision::Ask
                        && (*mode == PermissionModeDescriptor::Yolo
                            || (*mode == PermissionModeDescriptor::Strict
                                && !requires_interactive_approval(
                                    request,
                                    behavior,
                                    safe_listed,
                                    semantics.is_some(),
                                )))
                    {
                        PermissionDecision::Allow
                    } else {
                        configured
                    }
                }
            }
        }
    }

    fn yolo_active(&self) -> bool {
        *lock_read(&self.runtime_mode) == Some(PermissionModeDescriptor::Yolo)
            || matches!(
                &self.policy,
                PermissionPolicy::Headless(PermissionModeDescriptor::Yolo)
            )
    }

    fn is_safe_listed_bash(&self, request: &PermissionRequest, behavior: ToolBehavior) -> bool {
        behavior == ToolBehavior::Shell
            && bash_sandbox_mode(request) == Some(BashSandboxMode::Sandboxed)
            && request
                .arguments
                .get("network_domains")
                .is_none_or(|domains| {
                    normalize_network_domains(domains).is_some_and(|domains| domains.is_empty())
                })
            && request
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| {
                    self.command_safety.classify(command) == CommandSafety::SafeListed
                })
    }

    fn auto_safe_decision(
        &self,
        request: &PermissionRequest,
        semantics: Option<&ToolInvocationSemantics>,
        behavior: ToolBehavior,
        rules: Vec<PermissionRule>,
        safe_listed: bool,
    ) -> PermissionDecision {
        let configured = rule_decision(
            &PermissionConfig {
                default: PermissionDecision::Ask,
                rules,
            },
            request,
            behavior,
        );
        if configured == PermissionDecision::Deny {
            return PermissionDecision::Deny;
        }
        let memory = lock_read(&self.memory);
        if safe_listed
            || is_read_only(request, behavior)
            || is_auto_safe_workspace_write(request, semantics, &memory.workspace_roots)
        {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny
        }
    }
}

#[derive(Clone, Debug)]
enum PermissionPolicy {
    Configured(PermissionConfig),
    Headless(PermissionModeDescriptor),
}

fn root_yolo_footgun(is_root: bool, roots: &[PathBuf]) -> bool {
    is_root && roots.iter().any(|root| root == Path::new("/"))
}

const fn permission_mode_default(mode: PermissionModeDescriptor) -> PermissionDecision {
    match mode {
        PermissionModeDescriptor::Strict => PermissionDecision::Ask,
        PermissionModeDescriptor::AutoSafe => PermissionDecision::Deny,
        PermissionModeDescriptor::Yolo => PermissionDecision::Allow,
    }
}

fn requires_interactive_approval(
    request: &PermissionRequest,
    behavior: ToolBehavior,
    safe_listed_bash: bool,
    registered_semantics: bool,
) -> bool {
    if behavior == ToolBehavior::Shell {
        return !safe_listed_bash;
    }
    if !registered_semantics {
        return request
            .capabilities
            .iter()
            .any(|capability| !matches!(capability, ToolCapability::ReadFilesystem));
    }
    request
        .capabilities
        .contains(&ToolCapability::WriteFilesystem)
}

fn lock_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_mutex<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

mod identity;
use identity::{
    PermissionKey, RememberedApproval, bash_sandbox_mode, canonical_key_arguments_for,
    canonical_webfetch_origin, canonical_workspace_roots, contains_approval, fingerprint,
    is_auto_safe_workspace_write, is_builtin_read_only_bash, is_read_only,
    normalize_network_domains, rememberable_request, replace_approval, revoke_approvals,
    workspace_namespace,
};

mod rules;
use rules::{
    canonical_json, is_assignment, rule_decision, unsandboxed_rule_decision, validate_rule,
};

mod project_store;
use project_store::{ProjectApprovalStore, hex, shared_project_store};

#[cfg(test)]
mod tests;
