use std::fmt::Write as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
};

use async_trait::async_trait;
use globset::GlobBuilder;
use rw_tools::{BashSandboxMode, CommandSafety, CommandSafetyClassifier, classify_safe_command};
use rw_types::{
    ApprovalDecision, SessionMode, ToolCapability, UnifiedDiff,
    config::{PermissionConfig, PermissionDecision, PermissionRule},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// One tool invocation presented to the permission chokepoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PermissionRequest {
    pub id: String,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeadlessPermissionMode {
    Strict,
    AutoSafe,
    Yolo,
}

#[async_trait]
pub trait PermissionApprover: Send + Sync {
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision;
}

/// Introspection returned by `/permissions` without exposing filesystem internals.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PermissionSnapshot {
    pub default: PermissionDecision,
    pub runtime_mode: Option<HeadlessPermissionMode>,
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
    runtime_mode: Arc<RwLock<Option<HeadlessPermissionMode>>>,
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
    pub fn for_headless_mode(mode: HeadlessPermissionMode) -> Self {
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
            PermissionPolicy::Headless(HeadlessPermissionMode::Strict) => {
                (PermissionDecision::Ask, Vec::new())
            }
            PermissionPolicy::Headless(HeadlessPermissionMode::AutoSafe) => {
                (PermissionDecision::Deny, Vec::new())
            }
            PermissionPolicy::Headless(HeadlessPermissionMode::Yolo) => {
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
    pub fn set_runtime_mode(&self, mode: Option<HeadlessPermissionMode>) -> Result<(), String> {
        if matches!(self.policy, PermissionPolicy::Headless(_)) {
            return Err(
                "permission mode is fixed by the process launch policy and cannot be changed in this session"
                    .to_owned(),
            );
        }
        if mode == Some(HeadlessPermissionMode::Yolo) {
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

    pub(crate) fn execution_identity(request: &PermissionRequest) -> String {
        fingerprint(
            b"rottweiler-permission-execution-identity-v1\0",
            canonical_key_arguments(request).as_bytes(),
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
        if request.arguments.get("network_domains").is_some()
            && normalize_network_domains(&request.arguments["network_domains"]).is_none()
        {
            return PermissionOutcome::Denied;
        }
        if request.tool_name == "bash" && bash_sandbox_mode(&request).is_none() {
            return PermissionOutcome::Denied;
        }
        if request.tool_name == "webfetch"
            && request
                .arguments
                .get("url")
                .and_then(Value::as_str)
                .and_then(canonical_webfetch_origin)
                .is_none()
        {
            return PermissionOutcome::Denied;
        }
        if request.tool_name == "submit_plan" && mode != SessionMode::Plan {
            return PermissionOutcome::Denied;
        }
        if mode != SessionMode::Execute
            && !(is_read_only(&request) || is_builtin_read_only_bash(&request))
        {
            return PermissionOutcome::Denied;
        }
        if ask_override == Some(PermissionOutcome::Denied) {
            return PermissionOutcome::Denied;
        }
        match self.decision_for(&request) {
            PermissionDecision::Allow => PermissionOutcome::Allowed,
            PermissionDecision::Deny => PermissionOutcome::Denied,
            PermissionDecision::Ask => {
                if let Some(outcome) = ask_override {
                    return outcome;
                }
                let rememberable = rememberable_request(&request);
                let (key, generation, remembered) = {
                    let memory = lock_read(&self.memory);
                    let key = PermissionKey::from_request(&request, &memory.workspace_namespace);
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
    fn decision_for(&self, request: &PermissionRequest) -> PermissionDecision {
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
            ) != PermissionDecision::Allow
        }) {
            return PermissionDecision::Deny;
        }
        if matches!(request.tool_name.as_str(), "ask_user" | "submit_plan")
            && request.capabilities.is_empty()
        {
            return PermissionDecision::Allow;
        }
        let unsandboxed = bash_sandbox_mode(request) == Some(BashSandboxMode::Unsandboxed);
        let safe_listed = self.is_safe_listed_bash(request);
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
                let configured = rule_decision(&effective, request);
                if unsandboxed {
                    let explicit = unsandboxed_rule_decision(&effective, request);
                    if configured == PermissionDecision::Deny
                        || explicit == Some(PermissionDecision::Deny)
                    {
                        PermissionDecision::Deny
                    } else if explicit == Some(PermissionDecision::Allow)
                        || runtime_mode == Some(HeadlessPermissionMode::Yolo)
                    {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Ask
                    }
                } else if runtime_mode == Some(HeadlessPermissionMode::AutoSafe) {
                    self.auto_safe_decision(request, effective.rules, safe_listed)
                } else if configured == PermissionDecision::Ask
                    && (runtime_mode == Some(HeadlessPermissionMode::Yolo)
                        || !requires_interactive_approval(request, safe_listed))
                {
                    PermissionDecision::Allow
                } else {
                    configured
                }
            }
            PermissionPolicy::Headless(mode) => {
                let default = match mode {
                    HeadlessPermissionMode::Strict => PermissionDecision::Ask,
                    HeadlessPermissionMode::AutoSafe => PermissionDecision::Deny,
                    HeadlessPermissionMode::Yolo => PermissionDecision::Allow,
                };
                let rules = lock_read(&self.session_rules).clone();
                if unsandboxed {
                    let policy = PermissionConfig { default, rules };
                    let configured = rule_decision(&policy, request);
                    let explicit = unsandboxed_rule_decision(&policy, request);
                    return match mode {
                        HeadlessPermissionMode::AutoSafe => PermissionDecision::Deny,
                        _ if configured == PermissionDecision::Deny
                            || explicit == Some(PermissionDecision::Deny) =>
                        {
                            PermissionDecision::Deny
                        }
                        HeadlessPermissionMode::Strict
                            if explicit == Some(PermissionDecision::Allow) =>
                        {
                            PermissionDecision::Allow
                        }
                        HeadlessPermissionMode::Strict => PermissionDecision::Ask,
                        HeadlessPermissionMode::Yolo => PermissionDecision::Allow,
                    };
                }
                if *mode == HeadlessPermissionMode::AutoSafe {
                    return self.auto_safe_decision(request, rules, safe_listed);
                }
                if rules.is_empty() {
                    match mode {
                        HeadlessPermissionMode::Strict
                            if !requires_interactive_approval(request, safe_listed) =>
                        {
                            PermissionDecision::Allow
                        }
                        HeadlessPermissionMode::AutoSafe
                            if safe_listed || is_read_only(request) =>
                        {
                            PermissionDecision::Allow
                        }
                        _ => default,
                    }
                } else {
                    let configured = rule_decision(&PermissionConfig { default, rules }, request);
                    if configured == PermissionDecision::Ask
                        && (*mode == HeadlessPermissionMode::Yolo
                            || (*mode == HeadlessPermissionMode::Strict
                                && !requires_interactive_approval(request, safe_listed)))
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
        *lock_read(&self.runtime_mode) == Some(HeadlessPermissionMode::Yolo)
            || matches!(
                &self.policy,
                PermissionPolicy::Headless(HeadlessPermissionMode::Yolo)
            )
    }

    fn is_safe_listed_bash(&self, request: &PermissionRequest) -> bool {
        request.tool_name == "bash"
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
        rules: Vec<PermissionRule>,
        safe_listed: bool,
    ) -> PermissionDecision {
        let configured = rule_decision(
            &PermissionConfig {
                default: PermissionDecision::Ask,
                rules,
            },
            request,
        );
        if configured == PermissionDecision::Deny {
            return PermissionDecision::Deny;
        }
        let memory = lock_read(&self.memory);
        if safe_listed
            || is_read_only(request)
            || is_auto_safe_workspace_write(request, &memory.workspace_roots)
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
    Headless(HeadlessPermissionMode),
}

fn root_yolo_footgun(is_root: bool, roots: &[PathBuf]) -> bool {
    is_root && roots.iter().any(|root| root == Path::new("/"))
}

const fn permission_mode_default(mode: HeadlessPermissionMode) -> PermissionDecision {
    match mode {
        HeadlessPermissionMode::Strict => PermissionDecision::Ask,
        HeadlessPermissionMode::AutoSafe => PermissionDecision::Deny,
        HeadlessPermissionMode::Yolo => PermissionDecision::Allow,
    }
}

fn requires_interactive_approval(request: &PermissionRequest, safe_listed_bash: bool) -> bool {
    if request.tool_name == "bash" {
        return !safe_listed_bash;
    }
    request
        .capabilities
        .contains(&ToolCapability::WriteFilesystem)
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct PermissionKey {
    tool_name: String,
    arguments_fingerprint: String,
    capabilities: Vec<String>,
    approval_fingerprint: Option<String>,
    workspace_namespace: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct RememberedApproval {
    id: String,
    key: PermissionKey,
}

impl RememberedApproval {
    fn new(scope: &str, key: PermissionKey) -> Option<Self> {
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).ok()?;
        let opaque = hex(&random);
        Some(Self {
            id: format!("{scope}:{opaque}"),
            key,
        })
    }

    fn summary(&self) -> PermissionApprovalSummary {
        let capabilities = if self.key.capabilities.is_empty() {
            "none".to_owned()
        } else {
            self.key.capabilities.join(",")
        };
        let approval = self
            .key
            .approval_fingerprint
            .as_ref()
            .map_or("none", |_| "diff-bound");
        PermissionApprovalSummary {
            id: self.id.clone(),
            tool_name: self.key.tool_name.clone(),
            canonical_summary: format!(
                "exact-invocation=hidden capabilities={capabilities} approval={approval}"
            ),
        }
    }
}

impl PermissionKey {
    fn from_request(request: &PermissionRequest, workspace_namespace: &[String]) -> Self {
        let mut capabilities = request
            .capabilities
            .iter()
            .map(|capability| format!("{capability:?}"))
            .collect::<Vec<_>>();
        capabilities.sort();
        capabilities.dedup();
        Self {
            tool_name: request.tool_name.clone(),
            arguments_fingerprint: fingerprint(
                b"rottweiler-permission-arguments-v1\0",
                canonical_key_arguments(request).as_bytes(),
            ),
            capabilities,
            approval_fingerprint: request.approval_diff.as_ref().map(|diff| {
                format!(
                    "{}:{}:{}:{}",
                    diff.arguments_hash, diff.base_hash, diff.diff_hash, diff.truncated
                )
            }),
            workspace_namespace: workspace_namespace.to_vec(),
        }
    }
}

fn fingerprint(domain: &[u8], value: &[u8]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(domain);
    hash.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hash.update(value);
    hash.finalize().to_hex().to_string()
}

fn revoke_approvals(approvals: &mut BTreeSet<RememberedApproval>, id: Option<&str>) -> usize {
    let before = approvals.len();
    if let Some(id) = id {
        approvals.retain(|approval| approval.id != id);
    } else {
        approvals.clear();
    }
    before.saturating_sub(approvals.len())
}

fn contains_approval(approvals: &BTreeSet<RememberedApproval>, key: &PermissionKey) -> bool {
    approvals.iter().any(|approval| &approval.key == key)
}

fn replace_approval(approvals: &mut BTreeSet<RememberedApproval>, approval: RememberedApproval) {
    approvals.retain(|existing| existing.key != approval.key);
    approvals.insert(approval);
}

fn workspace_namespace(roots: impl IntoIterator<Item = impl AsRef<Path>>) -> Vec<String> {
    let mut namespace = blake3::Hasher::new();
    namespace.update(b"rottweiler-permission-workspace-roots-v1\0");
    let mut count = 0_u64;
    for root in roots {
        let canonical =
            fs::canonicalize(root.as_ref()).unwrap_or_else(|_| root.as_ref().to_path_buf());
        let bytes = canonical.as_os_str().as_encoded_bytes();
        namespace.update(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
        namespace.update(bytes);
        count = count.saturating_add(1);
    }
    namespace.update(&count.to_le_bytes());
    vec![namespace.finalize().to_hex().to_string()]
}

fn canonical_workspace_roots(roots: impl IntoIterator<Item = impl AsRef<Path>>) -> Vec<PathBuf> {
    roots
        .into_iter()
        .map(|root| fs::canonicalize(root.as_ref()).unwrap_or_else(|_| root.as_ref().to_path_buf()))
        .collect()
}

fn is_auto_safe_workspace_write(request: &PermissionRequest, roots: &[PathBuf]) -> bool {
    if !matches!(request.tool_name.as_str(), "write" | "edit" | "multi_edit")
        || roots.is_empty()
        || !request
            .capabilities
            .contains(&ToolCapability::WriteFilesystem)
        || request.capabilities.iter().any(|capability| {
            matches!(
                capability,
                ToolCapability::Execute | ToolCapability::Network
            )
        })
    {
        return false;
    }
    let Some(path) = request.arguments.get("path").and_then(Value::as_str) else {
        return false;
    };
    resolve_workspace_write_path(roots, path).is_some()
}

fn resolve_workspace_write_path(roots: &[PathBuf], supplied: &str) -> Option<PathBuf> {
    let supplied = Path::new(supplied);
    let candidate = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        let mut components = supplied.components();
        if components.next().is_some_and(
            |component| matches!(component, std::path::Component::Normal(name) if name == "@root"),
        ) {
            let std::path::Component::Normal(index) = components.next()? else {
                return None;
            };
            let index = index.to_str()?.parse::<usize>().ok()?;
            roots.get(index)?.join(components.collect::<PathBuf>())
        } else {
            roots.first()?.join(supplied)
        }
    };
    let canonical = canonicalize_with_missing_tail(&candidate)?;
    roots
        .iter()
        .any(|root| canonical.starts_with(root))
        .then_some(canonical)
}

fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut tail = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in tail.iter().rev() {
                    canonical.push(component);
                }
                return Some(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tail.push(ancestor.file_name()?.to_owned());
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
}

fn canonical_key_arguments(request: &PermissionRequest) -> String {
    let mut arguments = request.arguments.clone();
    if request.tool_name == "webfetch"
        && let Some(url) = arguments.get("url").and_then(Value::as_str)
        && let Some(origin) = canonical_webfetch_origin(url)
        && let Some(object) = arguments.as_object_mut()
    {
        object.insert("url".to_owned(), Value::String(origin));
    }
    if request.tool_name == "bash"
        && let Some(command) = arguments.get("command").and_then(Value::as_str)
        && let Some(commands) = exact_shell_identity(command, &arguments)
        && let Some(object) = arguments.as_object_mut()
    {
        object.insert(
            "command".to_owned(),
            serde_json::to_value(commands).unwrap_or(Value::Null),
        );
    }
    if let Some(object) = arguments.as_object_mut()
        && let Some(domains) = object.get("network_domains")
        && let Some(domains) = normalize_network_domains(domains)
    {
        object.insert(
            "network_domains".to_owned(),
            Value::Array(domains.into_iter().map(Value::String).collect()),
        );
    }
    canonical_json(&arguments)
}

#[derive(Serialize)]
struct ExactShellCommand {
    operator_after: Option<String>,
    assignments: Vec<(String, String)>,
    argv: Vec<String>,
    executable: ExactExecutableIdentity,
}

#[derive(Serialize)]
struct ExactExecutableIdentity {
    requested: String,
    resolved: Option<ResolvedExecutableIdentity>,
}

#[derive(Serialize)]
struct ResolvedExecutableIdentity {
    canonical_path: Vec<u8>,
    content_hash: String,
    trusted_immutable: bool,
}

fn exact_shell_identity(command: &str, arguments: &Value) -> Option<Vec<ExactShellCommand>> {
    let cwd = arguments
        .get("cwd")
        .and_then(Value::as_str)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let request_env = arguments.get("env").and_then(Value::as_object);
    split_compound_with_operators(command)?
        .into_iter()
        .map(|(segment, operator_after)| {
            let words = shell_words::split(&segment).ok()?;
            let executable_index = words.iter().position(|word| !is_assignment(word))?;
            let assignments = words[..executable_index]
                .iter()
                .map(|assignment| assignment.split_once('='))
                .map(|assignment| {
                    assignment.map(|(name, value)| (name.to_owned(), value.to_owned()))
                })
                .collect::<Option<Vec<_>>>()?;
            let argv = words[executable_index..].to_vec();
            let requested = argv.first()?.clone();
            let inline_path = assignments
                .iter()
                .rev()
                .find_map(|(name, value)| (name == "PATH").then_some(value.as_str()));
            let request_path = request_env
                .and_then(|env| env.get("PATH"))
                .and_then(Value::as_str);
            let inherited_path = std::env::var_os("PATH");
            let path = inline_path
                .map(std::ffi::OsString::from)
                .or_else(|| request_path.map(std::ffi::OsString::from))
                .or(inherited_path);
            Some(ExactShellCommand {
                operator_after,
                assignments,
                argv,
                executable: ExactExecutableIdentity {
                    requested: requested.clone(),
                    resolved: resolve_executable_identity(&requested, &cwd, path.as_deref()),
                },
            })
        })
        .collect()
}

fn resolve_executable_identity(
    executable: &str,
    cwd: &Path,
    path: Option<&std::ffi::OsStr>,
) -> Option<ResolvedExecutableIdentity> {
    let executable_path = Path::new(executable);
    let candidates = if executable_path.components().count() > 1 || executable_path.is_absolute() {
        vec![if executable_path.is_absolute() {
            executable_path.to_path_buf()
        } else {
            cwd.join(executable_path)
        }]
    } else {
        path.map(std::env::split_paths)
            .into_iter()
            .flatten()
            .map(|directory| {
                if directory.is_absolute() {
                    directory.join(executable_path)
                } else {
                    cwd.join(directory).join(executable_path)
                }
            })
            .collect()
    };
    for candidate in candidates {
        let Ok(canonical) = fs::canonicalize(&candidate) else {
            continue;
        };
        if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let Ok(mut file) = fs::File::open(&canonical) else {
            continue;
        };
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = std::io::Read::read(&mut file, &mut buffer).ok()?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        return Some(ResolvedExecutableIdentity {
            canonical_path: canonical.as_os_str().as_encoded_bytes().to_vec(),
            content_hash: hasher.finalize().to_hex().to_string(),
            trusted_immutable: trusted_immutable_executable(&canonical),
        });
    }
    None
}

#[cfg(unix)]
fn trusted_immutable_executable(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    fs::metadata(path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0
    })
}

#[cfg(not(unix))]
fn trusted_immutable_executable(_path: &Path) -> bool {
    false
}

fn rememberable_request(request: &PermissionRequest) -> bool {
    if request.tool_name != "bash" {
        return true;
    }
    let Some(command) = request.arguments.get("command").and_then(Value::as_str) else {
        return false;
    };
    if command.contains(['`', '$', '*', '?', '[', ']', '{', '}', '~', '\r']) {
        return false;
    }
    let Some(commands) = exact_shell_identity(command, &request.arguments) else {
        return false;
    };
    !commands.is_empty() && commands.iter().all(rememberable_shell_command)
}

fn rememberable_shell_command(command: &ExactShellCommand) -> bool {
    if command.assignments.iter().any(|(name, _)| name == "PATH") {
        return false;
    }
    let executable = Path::new(&command.executable.requested)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(
        executable,
        "eval" | "cd" | "export" | "unset" | "source" | "." | "alias" | "unalias" | "set" | "exec"
    ) {
        return false;
    }
    let interpreter = matches!(
        executable,
        "sh" | "bash" | "zsh" | "dash" | "python" | "python3" | "node" | "ruby" | "perl"
    );
    if interpreter && command.argv.iter().skip(1).any(|argument| argument == "-c") {
        return false;
    }
    command
        .executable
        .resolved
        .as_ref()
        .is_some_and(|identity| identity.trusted_immutable)
}

fn split_compound_with_operators(command: &str) -> Option<Vec<(String, Option<String>)>> {
    let chars = command.char_indices().collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let (offset, character) = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if character == '\\' && !single {
            escaped = true;
            index += 1;
            continue;
        }
        if character == '\'' && !double {
            single = !single;
            index += 1;
            continue;
        }
        if character == '"' && !single {
            double = !double;
            index += 1;
            continue;
        }
        if !single && !double {
            let next = chars.get(index + 1).map(|(_, next)| *next);
            let operator = match (character, next) {
                ('&', Some('&')) => Some(("&&", 2)),
                ('|', Some('|')) => Some(("||", 2)),
                (';', _) => Some((";", 1)),
                ('|', _) => Some(("|", 1)),
                ('\n', _) => Some(("\n", 1)),
                ('&' | '(' | ')' | '<' | '>', _) => return None,
                _ => None,
            };
            if let Some((operator, delimiter_len)) = operator {
                let segment = command.get(start..offset)?.trim();
                if segment.is_empty() {
                    return None;
                }
                segments.push((segment.to_owned(), Some(operator.to_owned())));
                index += delimiter_len;
                start = chars.get(index).map_or(command.len(), |(next, _)| *next);
                continue;
            }
        }
        index += 1;
    }
    if single || double || escaped {
        return None;
    }
    let tail = command.get(start..)?.trim();
    if tail.is_empty() {
        return None;
    }
    segments.push((tail.to_owned(), None));
    Some(segments)
}

fn canonical_webfetch_origin(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https")
        .then(|| url.origin().ascii_serialization())
        .filter(|origin| origin != "null")
}

fn normalize_network_domains(value: &Value) -> Option<Vec<String>> {
    let mut normalized = value
        .as_array()?
        .iter()
        .map(|domain| {
            let domain = domain
                .as_str()?
                .trim()
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if domain.is_empty()
                || domain.len() > 253
                || domain.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
            {
                None
            } else {
                Some(domain)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    normalized.sort();
    normalized.dedup();
    Some(normalized)
}

fn is_read_only(request: &PermissionRequest) -> bool {
    (request.capabilities.is_empty()
        && matches!(request.tool_name.as_str(), "ask_user" | "submit_plan"))
        || (!request.capabilities.is_empty()
            && request
                .capabilities
                .iter()
                .all(|capability| matches!(capability, ToolCapability::ReadFilesystem)))
}

fn bash_sandbox_mode(request: &PermissionRequest) -> Option<BashSandboxMode> {
    match request.arguments.get("sandbox") {
        None => Some(BashSandboxMode::Sandboxed),
        Some(Value::String(mode)) if mode == "sandboxed" => Some(BashSandboxMode::Sandboxed),
        Some(Value::String(mode)) if mode == "unsandboxed" => Some(BashSandboxMode::Unsandboxed),
        Some(_) => None,
    }
}

fn is_builtin_read_only_bash(request: &PermissionRequest) -> bool {
    request.tool_name == "bash"
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
            .is_some_and(|command| classify_safe_command(command) == CommandSafety::SafeListed)
}

fn rule_decision(config: &PermissionConfig, request: &PermissionRequest) -> PermissionDecision {
    let Some(targets) = canonical_arguments(request) else {
        return config.default;
    };
    let mut all_allowed = !targets.is_empty();
    let mut any_asked = false;
    for target in targets {
        let mut target_decision = None;
        for rule in &config.rules {
            let Some((tool, pattern)) = parse_rule(&rule.pattern) else {
                continue;
            };
            if tool != request.tool_name || !glob_matches(pattern, &target) {
                continue;
            }
            if rule.action == PermissionDecision::Deny {
                return PermissionDecision::Deny;
            }
            target_decision = Some(rule.action);
        }
        if target_decision == Some(PermissionDecision::Ask) {
            any_asked = true;
        }
        if target_decision != Some(PermissionDecision::Allow) {
            all_allowed = false;
        }
    }
    if any_asked {
        PermissionDecision::Ask
    } else if all_allowed {
        if request
            .arguments
            .get("network_domains")
            .and_then(normalize_network_domains)
            .is_some_and(|domains| !domains.is_empty())
        {
            capability_rule_decision(config, "network", &request.tool_name)
                .unwrap_or(config.default)
        } else {
            PermissionDecision::Allow
        }
    } else {
        config.default
    }
}

/// Returns authority from the explicit `bash_unsandboxed(pattern)` namespace.
/// Ordinary `bash(pattern)` allows never imply permission to bypass the native
/// sandbox; their deny decisions are still honored by `decision_for`.
fn unsandboxed_rule_decision(
    config: &PermissionConfig,
    request: &PermissionRequest,
) -> Option<PermissionDecision> {
    if request.tool_name != "bash"
        || bash_sandbox_mode(request) != Some(BashSandboxMode::Unsandboxed)
    {
        return None;
    }
    let targets = canonical_arguments(request)?;
    let mut all_allowed = !targets.is_empty();
    let mut any_asked = false;
    let mut any_matched = false;
    for target in targets {
        let mut target_decision = None;
        for rule in &config.rules {
            let Some((tool, pattern)) = parse_rule(&rule.pattern) else {
                continue;
            };
            if tool != "bash_unsandboxed" || !glob_matches(pattern, &target) {
                continue;
            }
            any_matched = true;
            if rule.action == PermissionDecision::Deny {
                return Some(PermissionDecision::Deny);
            }
            target_decision = Some(rule.action);
        }
        any_asked |= target_decision == Some(PermissionDecision::Ask);
        all_allowed &= target_decision == Some(PermissionDecision::Allow);
    }
    if any_asked {
        Some(PermissionDecision::Ask)
    } else if all_allowed && any_matched {
        Some(PermissionDecision::Allow)
    } else {
        None
    }
}

fn validate_rule(rule: &str) -> Result<(), String> {
    let Some((tool, pattern)) = parse_rule(rule) else {
        return Err("permission rule must use tool(glob) syntax".to_owned());
    };
    if !tool
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err("permission rule tool names use letters, digits, `_`, or `-`".to_owned());
    }
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .map(|_| ())
        .map_err(|error| format!("invalid permission glob: {error}"))
}

fn capability_rule_decision(
    config: &PermissionConfig,
    capability: &str,
    tool_name: &str,
) -> Option<PermissionDecision> {
    let mut decision = None;
    for rule in &config.rules {
        let Some((tool, pattern)) = parse_rule(&rule.pattern) else {
            continue;
        };
        if tool != capability || !glob_matches(pattern, tool_name) {
            continue;
        }
        if rule.action == PermissionDecision::Deny {
            return Some(PermissionDecision::Deny);
        }
        decision = Some(rule.action);
    }
    decision
}

fn parse_rule(rule: &str) -> Option<(&str, &str)> {
    let open = rule.find('(')?;
    let tool = rule[..open].trim();
    let pattern = rule.get(open + 1..rule.len().checked_sub(1)?)?;
    (!tool.is_empty() && rule.ends_with(')')).then_some((tool, pattern))
}

fn glob_matches(pattern: &str, target: &str) -> bool {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(target))
}

fn canonical_arguments(request: &PermissionRequest) -> Option<Vec<String>> {
    if request.tool_name == "bash" {
        return request
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .and_then(canonical_shell_commands);
    }
    for key in ["path", "url", "domain", "command"] {
        if let Some(value) = request.arguments.get(key).and_then(Value::as_str) {
            return Some(vec![value.trim().to_owned()]);
        }
    }
    Some(vec![canonical_json(&request.arguments)])
}

fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

fn canonical_shell_commands(command: &str) -> Option<Vec<String>> {
    // Permission allow rules must bind the argv the process will actually
    // receive. Shell expansion happens after tokenization, so unresolved
    // variables, globs, braces, and tildes fall back to the configured default
    // instead of matching an allow rule over misleading literal text.
    if command.contains(['`', '$', '*', '?', '[', ']', '{', '}', '~']) {
        return None;
    }
    let segments = split_compound(command)?;
    let mut canonical = Vec::with_capacity(segments.len());
    for segment in segments {
        let mut argv = shell_words::split(segment.trim()).ok()?;
        if argv.is_empty() {
            return None;
        }
        let command_index = argv.iter().position(|argument| !is_assignment(argument))?;
        if command_index != 0 {
            return None;
        }
        let binary = Path::new(argv.first()?).file_name()?.to_str()?.to_owned();
        if binary == "eval"
            || (["bash", "sh", "zsh", "dash"].contains(&binary.as_str())
                && argv.iter().skip(1).any(|argument| argument == "-c"))
        {
            return None;
        }
        argv[0] = binary;
        if argv[0] == "rm" {
            normalize_rm_flags(&mut argv);
        }
        canonical.push(argv.join(" "));
    }
    (!canonical.is_empty()).then_some(canonical)
}

fn is_assignment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

fn normalize_rm_flags(argv: &mut Vec<String>) {
    let option_end = argv
        .iter()
        .skip(1)
        .position(|argument| !argument.starts_with('-') || argument == "-")
        .map_or(argv.len(), |index| index + 1);
    let mut flags = BTreeSet::new();
    let mut long = Vec::new();
    for option in argv.drain(1..option_end) {
        if option.starts_with("--") {
            long.push(option);
        } else {
            flags.extend(option.trim_start_matches('-').chars());
        }
    }
    let mut normalized = String::from("-");
    for preferred in ['r', 'f'] {
        if flags.remove(&preferred) {
            normalized.push(preferred);
        }
    }
    normalized.extend(flags);
    let mut insertion = Vec::new();
    if normalized.len() > 1 {
        insertion.push(normalized);
    }
    long.sort();
    insertion.extend(long);
    argv.splice(1..1, insertion);
}

fn split_compound(command: &str) -> Option<Vec<String>> {
    let chars = command.char_indices().collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let (offset, character) = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if character == '\\' && !single {
            escaped = true;
            index += 1;
            continue;
        }
        if character == '\'' && !double {
            single = !single;
            index += 1;
            continue;
        }
        if character == '"' && !single {
            double = !double;
            index += 1;
            continue;
        }
        if !single && !double {
            let next = chars.get(index + 1).map(|(_, c)| *c);
            let delimiter_len = match (character, next) {
                ('&', Some('&')) | ('|', Some('|')) => 2,
                (';' | '|' | '\n', _) => 1,
                ('&' | '(' | ')' | '<' | '>', _) => return None,
                _ => 0,
            };
            if delimiter_len > 0 {
                let segment = command.get(start..offset)?.trim();
                if segment.is_empty() {
                    return None;
                }
                segments.push(segment.to_owned());
                index += delimiter_len;
                start = chars.get(index).map_or(command.len(), |(next, _)| *next);
                continue;
            }
        }
        index += 1;
    }
    if single || double || escaped {
        return None;
    }
    let tail = command.get(start..)?.trim();
    if !tail.is_empty() {
        segments.push(tail.to_owned());
    }
    Some(segments)
}

struct ProjectApprovalStore {
    path: PathBuf,
    transaction: Mutex<()>,
    cached: RwLock<BTreeSet<RememberedApproval>>,
}

impl ProjectApprovalStore {
    fn refresh(&self) -> Result<BTreeSet<RememberedApproval>, std::io::Error> {
        let _transaction = lock_mutex(&self.transaction);
        let _file_lock = CrossProcessApprovalLock::acquire(&self.path)?;
        let approvals = load_project_approvals(&self.path)?;
        lock_write(&self.cached).clone_from(&approvals);
        Ok(approvals)
    }

    fn contains(&self, key: &PermissionKey) -> Result<bool, std::io::Error> {
        self.refresh()
            .map(|approvals| contains_approval(&approvals, key))
    }

    fn grant(&self, key: PermissionKey) -> Result<(), std::io::Error> {
        self.update(|approvals| {
            if !contains_approval(approvals, &key) {
                let approval = RememberedApproval::new("project", key).ok_or_else(|| {
                    std::io::Error::other("secure random approval id generation failed")
                })?;
                replace_approval(approvals, approval);
            }
            Ok(())
        })
    }

    fn revoke(&self, id: Option<&str>) -> Result<usize, std::io::Error> {
        let mut removed = 0;
        self.update(|approvals| {
            removed = revoke_approvals(approvals, id);
            Ok(())
        })?;
        Ok(removed)
    }

    fn clear_all(&self) -> Result<usize, std::io::Error> {
        self.revoke(None)
    }

    fn update(
        &self,
        change: impl FnOnce(&mut BTreeSet<RememberedApproval>) -> Result<(), std::io::Error>,
    ) -> Result<(), std::io::Error> {
        let _transaction = lock_mutex(&self.transaction);
        let _file_lock = CrossProcessApprovalLock::acquire(&self.path)?;
        let mut approvals = load_project_approvals(&self.path)?;
        let original = approvals.clone();
        change(&mut approvals)?;
        if approvals != original {
            persist_project_approvals(&self.path, &approvals)?;
        }
        *lock_write(&self.cached) = approvals;
        Ok(())
    }
}

fn shared_project_store(path: &Path) -> Arc<ProjectApprovalStore> {
    static STORES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<ProjectApprovalStore>>>> = OnceLock::new();
    let normalized = normalize_approval_path(path);
    let registry = STORES.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut registry = lock_mutex(registry);
    Arc::clone(registry.entry(normalized.clone()).or_insert_with(|| {
        let store = Arc::new(ProjectApprovalStore {
            path: normalized,
            transaction: Mutex::new(()),
            cached: RwLock::new(BTreeSet::new()),
        });
        let _ = store.refresh();
        store
    }))
}

fn normalize_approval_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let Some(parent) = absolute.parent() else {
        return absolute;
    };
    fs::canonicalize(parent)
        .map(|parent| parent.join(absolute.file_name().unwrap_or_default()))
        .unwrap_or(absolute)
}

struct CrossProcessApprovalLock {
    _file: fs::File,
}

impl CrossProcessApprovalLock {
    fn acquire(path: &Path) -> Result<Self, std::io::Error> {
        #[cfg(not(unix))]
        {
            let _ = path;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "durable project approvals require a supported cross-process file lock",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            fs::create_dir_all(parent)?;
            set_private_directory(parent)?;
            let lock_path = sibling_path(path, "lock")?;
            let mut options = fs::OpenOptions::new();
            options.read(true).write(true).create(true);
            options
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
            let file = options.open(lock_path)?;
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
            Ok(Self { _file: file })
        }
    }
}

fn load_project_approvals(path: &Path) -> Result<BTreeSet<RememberedApproval>, std::io::Error> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "project approval ledger is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "project approval ledger is not private",
            ));
        }
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("project approval ledger is malformed: {error}"),
        )
    })
}

fn persist_project_approvals(
    path: &Path,
    approvals: &BTreeSet<RememberedApproval>,
) -> Result<(), std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    set_private_directory(parent)?;
    let encoded = serde_json::to_vec(approvals)
        .map_err(|error| std::io::Error::other(format!("approval encoding failed: {error}")))?;
    let temporary = unique_temporary_path(path)?;
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        if let Err(error) = fs::File::open(parent).and_then(|directory| directory.sync_all()) {
            let _ = fs::remove_file(path);
            let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
            return Err(error);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn set_private_directory(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn unique_temporary_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| std::io::Error::other("secure random temp name generation failed"))?;
    let suffix = hex(&random);
    sibling_path(path, &format!("tmp.{suffix}"))
}

fn sibling_path(path: &Path, suffix: &str) -> Result<PathBuf, std::io::Error> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "approval ledger has no file name",
        )
    })?;
    let mut sibling = name.to_os_string();
    sibling.push(format!(".{suffix}"));
    Ok(path.with_file_name(sibling))
}

fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Decision(ApprovalDecision);

    #[async_trait]
    impl PermissionApprover for Decision {
        async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
            self.0.clone()
        }
    }

    struct CountingDeny(AtomicUsize);

    #[async_trait]
    impl PermissionApprover for CountingDeny {
        async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
            self.0.fetch_add(1, Ordering::SeqCst);
            ApprovalDecision::Deny
        }
    }

    struct ChangeWorkspaceThenApprove {
        gate: Arc<PermissionGate>,
        replacement: PathBuf,
        decision: ApprovalDecision,
    }

    #[async_trait]
    impl PermissionApprover for ChangeWorkspaceThenApprove {
        async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
            self.gate
                .replace_workspace_roots([&self.replacement])
                .expect("workspace replacement");
            self.decision.clone()
        }
    }

    fn request(command: &str, capabilities: Vec<ToolCapability>) -> PermissionRequest {
        PermissionRequest {
            id: "call".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({ "command": command }),
            capabilities,
            approval_diff: None,
        }
    }

    fn bash_request(command: &str, cwd: &Path) -> PermissionRequest {
        PermissionRequest {
            id: "exact-bash".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({
                "command": command,
                "cwd": cwd,
                "env": {},
                "network_domains": [],
            }),
            capabilities: vec![ToolCapability::Execute],
            approval_diff: None,
        }
    }

    fn independent_project_store(path: &Path) -> ProjectApprovalStore {
        ProjectApprovalStore {
            path: path.to_path_buf(),
            transaction: Mutex::new(()),
            cached: RwLock::new(BTreeSet::new()),
        }
    }

    #[test]
    fn canonical_shell_requires_every_simple_command_and_normalizes_rm_flags() {
        assert_eq!(
            canonical_shell_commands("/usr/bin/git status && rm -fr build"),
            Some(vec!["git status".to_owned(), "rm -rf build".to_owned()])
        );
        assert!(canonical_shell_commands("bash -c 'git status'").is_none());
        assert!(canonical_shell_commands("echo $(cat secret)").is_none());
        for command in [
            "LD_PRELOAD=/tmp/injected.so cat file",
            "cat $FILE",
            "cat ${FILE}",
            "cat *.rs",
            "cat file?.rs",
            "cat [ab].rs",
            "printf {a,b}",
            "cat ~/secret",
        ] {
            assert!(
                canonical_shell_commands(command).is_none(),
                "runtime-expanded command matched an allow-rule target: {command}"
            );
        }
    }

    #[test]
    fn exact_shell_identity_preserves_assignments_argv_boundaries_order_and_operators() {
        let cwd = tempfile::tempdir().expect("cwd");
        let identity = |command| canonical_key_arguments(&bash_request(command, cwd.path()));
        assert_ne!(
            identity("FLAG=a /bin/echo x"),
            identity("FLAG=b /bin/echo x")
        );
        assert_ne!(identity("/bin/echo 'a b'"), identity("/bin/echo a b"));
        assert_ne!(
            identity("/bin/echo a && /bin/echo b"),
            identity("/bin/echo b && /bin/echo a")
        );
        assert_ne!(
            identity("/bin/echo a && /bin/echo b"),
            identity("/bin/echo a ; /bin/echo b")
        );
    }

    #[tokio::test]
    async fn per_turn_qualified_tool_restriction_denies_broader_bash_invocations() {
        let gate = PermissionGate::new(PermissionDecision::Allow)
            .restricted_to_patterns(&["bash(git status)".to_owned()])
            .expect("qualified restriction");
        let request = |command: &str| PermissionRequest {
            id: format!("bash-{command}"),
            tool_name: "bash".to_owned(),
            arguments: json!({
                "command": command,
                "cwd": ".",
                "env": {},
                "network_domains": [],
                "sandbox": "sandboxed",
            }),
            capabilities: vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
            approval_diff: None,
        };
        let deny = Decision(ApprovalDecision::Deny);
        assert_eq!(
            gate.authorize(request("git status"), &deny).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(request("git push"), &deny).await,
            PermissionOutcome::Denied
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exact_bash_session_and_project_approvals_do_not_collide() {
        let root = tempfile::tempdir().expect("tempdir");
        for (scope, decision) in [
            ("session", ApprovalDecision::AllowSession),
            ("project", ApprovalDecision::AllowProject),
        ] {
            let gate = PermissionGate::new(PermissionDecision::Ask)
                .with_workspace_roots([root.path()])
                .with_project_approval_file(root.path().join(format!("{scope}.json")));
            let approved = bash_request("/bin/echo safe", root.path());
            assert_eq!(
                gate.authorize(approved.clone(), &Decision(decision)).await,
                PermissionOutcome::Allowed
            );
            let deny = CountingDeny(AtomicUsize::new(0));
            for command in [
                "FLAG=changed /bin/echo safe",
                "/bin/echo 'safe value'",
                "/bin/echo safe && /bin/echo done",
                "/bin/echo done && /bin/echo safe",
            ] {
                assert_eq!(
                    gate.authorize(bash_request(command, root.path()), &deny)
                        .await,
                    PermissionOutcome::Denied,
                    "{scope} approval collided for {command}"
                );
            }
            assert_eq!(deny.0.load(Ordering::SeqCst), 4);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn complex_or_mutable_bash_approval_executes_once_and_is_never_remembered() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("tempdir");
        let script = root.path().join("script");
        fs::write(&script, "#!/bin/sh\nprintf mutable\n").expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("executable");
        for command in [
            "/bin/echo ok > output",
            "/bin/echo $(/bin/echo nested)",
            "/bin/rm *.tmp",
            "/bin/rm file?.tmp",
            "/bin/rm [ab].tmp",
            "/bin/echo {first,second}",
            "/bin/echo ~/secret",
            "/bin/echo background &",
            "/bin/sh -c '/bin/echo nested'",
            "eval /bin/echo unsafe",
            "cd /tmp",
            "export PATH=/tmp",
            "PATH=/tmp /bin/echo changed",
            "./script safe",
        ] {
            let gate = PermissionGate::new(PermissionDecision::Ask)
                .with_workspace_roots([root.path()])
                .with_project_approval_file(root.path().join(format!(
                    "complex-{}.json",
                    blake3::hash(command.as_bytes()).to_hex()
                )));
            let invocation = bash_request(command, root.path());
            assert_eq!(
                gate.authorize(
                    invocation.clone(),
                    &Decision(ApprovalDecision::AllowProject),
                )
                .await,
                PermissionOutcome::Allowed,
                "an accepted approval must execute the displayed invocation for {command}"
            );
            assert_eq!(gate.snapshot().project_approvals, 0);
            assert_eq!(gate.snapshot().session_approvals, 0);
            assert_eq!(
                gate.authorize(invocation.clone(), &Decision(ApprovalDecision::AllowOnce),)
                    .await,
                PermissionOutcome::Allowed,
                "one-time approval should remain usable for {command}"
            );
            assert_eq!(
                gate.authorize(invocation, &Decision(ApprovalDecision::Deny))
                    .await,
                PermissionOutcome::Denied,
                "non-rememberable command was recalled: {command}"
            );
        }
    }

    #[tokio::test]
    async fn nonrememberable_approval_is_denied_when_workspace_changes_while_prompting() {
        let roots = tempfile::tempdir().expect("roots");
        let initial = roots.path().join("initial");
        let replacement = roots.path().join("replacement");
        fs::create_dir(&initial).expect("initial root");
        fs::create_dir(&replacement).expect("replacement root");

        for decision in [
            ApprovalDecision::AllowSession,
            ApprovalDecision::AllowProject,
        ] {
            let gate = Arc::new(
                PermissionGate::new(PermissionDecision::Ask).with_workspace_roots([&initial]),
            );
            let approver = ChangeWorkspaceThenApprove {
                gate: Arc::clone(&gate),
                replacement: replacement.clone(),
                decision,
            };
            assert_eq!(
                gate.authorize(
                    bash_request("/bin/echo approved > output", &initial),
                    &approver,
                )
                .await,
                PermissionOutcome::Denied,
                "an approval from the old workspace generation must never execute"
            );
        }
    }

    #[test]
    fn session_approval_id_failure_degrades_to_allow_once() {
        let gate = PermissionGate::new(PermissionDecision::Ask);
        assert_eq!(
            gate.accept_for_generation(0, None),
            PermissionOutcome::Allowed,
            "failure to allocate remember-only metadata must not reject the accepted invocation"
        );
        assert_eq!(gate.snapshot().session_approvals, 0);
    }

    #[tokio::test]
    async fn unavailable_project_approval_persistence_degrades_to_allow_once() {
        let invocation = PermissionRequest {
            id: "write".to_owned(),
            tool_name: "write".to_owned(),
            arguments: json!({"path": "file.txt", "content": "content"}),
            capabilities: vec![ToolCapability::WriteFilesystem],
            approval_diff: None,
        };

        let without_store = PermissionGate::new(PermissionDecision::Ask);
        assert_eq!(
            without_store
                .authorize(
                    invocation.clone(),
                    &Decision(ApprovalDecision::AllowProject),
                )
                .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(without_store.snapshot().project_approvals, 0);
        assert_eq!(
            without_store
                .authorize(invocation.clone(), &Decision(ApprovalDecision::Deny))
                .await,
            PermissionOutcome::Denied,
            "the fallback must not accidentally become a remembered approval"
        );

        let root = tempfile::tempdir().expect("tempdir");
        let blocked_parent = root.path().join("not-a-directory");
        fs::write(&blocked_parent, "file").expect("blocking file");
        let failing_store = PermissionGate::new(PermissionDecision::Ask)
            .with_project_approval_file(blocked_parent.join("approvals.json"));
        assert_eq!(
            failing_store
                .authorize(
                    invocation.clone(),
                    &Decision(ApprovalDecision::AllowProject)
                )
                .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(failing_store.snapshot().project_approvals, 0);
        assert_eq!(
            failing_store
                .authorize(invocation, &Decision(ApprovalDecision::Deny))
                .await,
            PermissionOutcome::Denied
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn audited_root_owned_simple_executable_can_be_remembered() {
        let root = tempfile::tempdir().expect("tempdir");
        for decision in [
            ApprovalDecision::AllowSession,
            ApprovalDecision::AllowProject,
        ] {
            let gate = PermissionGate::new(PermissionDecision::Ask)
                .with_workspace_roots([root.path()])
                .with_project_approval_file(root.path().join(format!("{decision:?}.json")));
            let invocation = bash_request("/bin/echo stable", root.path());
            assert_eq!(
                gate.authorize(invocation.clone(), &Decision(decision))
                    .await,
                PermissionOutcome::Allowed
            );
            assert_eq!(
                gate.authorize(invocation, &Decision(ApprovalDecision::Deny))
                    .await,
                PermissionOutcome::Allowed
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn project_approval_without_portable_file_lock_degrades_to_allow_once() {
        let root = tempfile::tempdir().expect("tempdir");
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(root.path().join("approvals.json"));
        let request = PermissionRequest {
            id: "write".to_owned(),
            tool_name: "write".to_owned(),
            arguments: json!({"path": "file.txt", "content": "content"}),
            capabilities: vec![ToolCapability::WriteFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(request, &Decision(ApprovalDecision::AllowProject))
                .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(gate.snapshot().project_approvals, 0);
    }

    #[tokio::test]
    async fn compound_allow_requires_every_command_to_match() {
        let gate = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![PermissionRule {
                pattern: "bash(git status*)".to_owned(),
                action: PermissionDecision::Allow,
            }],
        });
        let read = vec![ToolCapability::ReadFilesystem];
        assert_eq!(
            gate.authorize(
                request("git status", read.clone()),
                &Decision(ApprovalDecision::Deny)
            )
            .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(
                request("git status && /bin/echo README", read),
                &Decision(ApprovalDecision::Deny)
            )
            .await,
            PermissionOutcome::Denied
        );
        for redirected in ["git status > changed", "git status 2>err"] {
            assert_eq!(
                gate.authorize(
                    request(redirected, vec![ToolCapability::ReadFilesystem]),
                    &Decision(ApprovalDecision::Deny)
                )
                .await,
                PermissionOutcome::Denied
            );
        }
        for expanded in [
            "MODE=unsafe git status",
            "git status $FLAGS",
            "git status *.rs",
            "git status ~/other-worktree",
        ] {
            assert_eq!(
                gate.authorize(
                    request(expanded, vec![ToolCapability::ReadFilesystem]),
                    &Decision(ApprovalDecision::Deny)
                )
                .await,
                PermissionOutcome::Denied,
                "runtime-expanded command bypassed the approval fallback: {expanded}"
            );
        }
    }

    #[tokio::test]
    async fn session_rules_add_replace_remove_and_clear_through_the_gate() {
        let gate = PermissionGate::new(PermissionDecision::Ask);
        let approver = CountingDeny(AtomicUsize::new(0));
        let invocation = || request("cargo publish --dry-run", vec![ToolCapability::Execute]);
        gate.add_session_rule(PermissionRule {
            pattern: "bash(cargo publish*)".to_owned(),
            action: PermissionDecision::Allow,
        })
        .expect("valid session rule");
        assert_eq!(
            gate.authorize(invocation(), &approver).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 0);
        assert_eq!(gate.snapshot().session_rules.len(), 1);

        gate.add_session_rule(PermissionRule {
            pattern: "bash(cargo publish*)".to_owned(),
            action: PermissionDecision::Deny,
        })
        .expect("replace session rule");
        assert_eq!(gate.snapshot().session_rules.len(), 1);
        assert_eq!(
            gate.authorize(invocation(), &approver).await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 0);

        assert!(gate.remove_session_rule("bash(cargo publish*)"));
        assert!(!gate.remove_session_rule("bash(cargo publish*)"));
        assert_eq!(
            gate.authorize(invocation(), &approver).await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 1);
        assert_eq!(gate.clear_session_rules(), 0);
        assert!(
            gate.add_session_rule(PermissionRule {
                pattern: "not a rule".to_owned(),
                action: PermissionDecision::Allow,
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn trusted_project_allows_read_only_tools_but_preserves_explicit_denies() {
        let root = tempfile::tempdir().expect("tempdir");
        let request = PermissionRequest {
            id: "trusted-glob".to_owned(),
            tool_name: "glob".to_owned(),
            arguments: json!({"pattern": "**/*.rs", "path": "."}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        let no_prompt = CountingDeny(AtomicUsize::new(0));
        let trusted = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_trusted_read_roots([root.path()]);
        assert_eq!(
            trusted.authorize(request.clone(), &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);

        let denied = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![PermissionRule {
                pattern: "glob(*)".to_owned(),
                action: PermissionDecision::Deny,
            }],
        })
        .with_workspace_roots([root.path()])
        .with_trusted_read_roots([root.path()]);
        assert_eq!(
            denied.authorize(request, &no_prompt).await,
            PermissionOutcome::Denied
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn trusted_workspace_allows_pathless_builtin_symbol_reads_only_with_full_authority() {
        let primary = tempfile::tempdir().expect("primary");
        let secondary = tempfile::tempdir().expect("secondary");
        let symbols = || PermissionRequest {
            id: "workspace-symbols".to_owned(),
            tool_name: "symbols".to_owned(),
            arguments: json!({"pattern": "ProviderRuntime"}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        let no_prompt = CountingDeny(AtomicUsize::new(0));
        let trusted = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([primary.path(), secondary.path()])
            .with_trusted_read_roots([primary.path(), secondary.path()]);
        assert_eq!(
            trusted.authorize(symbols(), &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);

        let partially_trusted = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([primary.path(), secondary.path()])
            .with_trusted_read_roots([primary.path()]);
        assert_eq!(
            partially_trusted.authorize(symbols(), &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn trusted_workspace_read_authority_rejects_extensions_network_and_explicit_denies() {
        let root = tempfile::tempdir().expect("workspace");
        let no_prompt = CountingDeny(AtomicUsize::new(0));
        let trusted = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_trusted_read_roots([root.path()]);
        let extension = PermissionRequest {
            id: "extension-read".to_owned(),
            tool_name: "extension_read".to_owned(),
            arguments: json!({"path": "."}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            trusted.authorize(extension, &no_prompt).await,
            PermissionOutcome::Allowed
        );

        let network = PermissionRequest {
            id: "network-symbols".to_owned(),
            tool_name: "symbols".to_owned(),
            arguments: json!({
                "pattern": "ProviderRuntime",
                "network_domains": ["example.com"]
            }),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            trusted.authorize(network, &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);

        let denied = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![PermissionRule {
                pattern: "symbols(*)".to_owned(),
                action: PermissionDecision::Deny,
            }],
        })
        .with_workspace_roots([root.path()])
        .with_trusted_read_roots([root.path()]);
        let symbols = PermissionRequest {
            id: "denied-symbols".to_owned(),
            tool_name: "symbols".to_owned(),
            arguments: json!({"pattern": "ProviderRuntime"}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            denied.authorize(symbols, &no_prompt).await,
            PermissionOutcome::Denied
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn trusted_read_only_authority_is_scoped_to_each_workspace_root() {
        let primary = tempfile::tempdir().expect("primary");
        let secondary = tempfile::tempdir().expect("secondary");
        std::fs::write(primary.path().join("primary.rs"), "primary").expect("primary fixture");
        std::fs::write(secondary.path().join("secondary.rs"), "secondary")
            .expect("secondary fixture");
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([primary.path(), secondary.path()])
            .with_trusted_read_roots([primary.path()]);
        let no_prompt = CountingDeny(AtomicUsize::new(0));

        let primary_read = PermissionRequest {
            id: "primary-read".to_owned(),
            tool_name: "read".to_owned(),
            arguments: json!({"path": "@root/0/primary.rs"}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(primary_read, &no_prompt).await,
            PermissionOutcome::Allowed
        );

        let secondary_read = PermissionRequest {
            id: "secondary-read".to_owned(),
            tool_name: "read".to_owned(),
            arguments: json!({"path": "@root/1/secondary.rs"}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(secondary_read, &no_prompt).await,
            PermissionOutcome::Allowed
        );

        let all_roots_glob = PermissionRequest {
            id: "all-roots-glob".to_owned(),
            tool_name: "glob".to_owned(),
            arguments: json!({"pattern": "**/*.rs", "path": "."}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(all_roots_glob, &no_prompt).await,
            PermissionOutcome::Allowed
        );
        let default_all_roots_ls = PermissionRequest {
            id: "default-all-roots-ls".to_owned(),
            tool_name: "ls".to_owned(),
            arguments: json!({"recursive": false}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(default_all_roots_ls, &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn trusted_secondary_root_allows_virtual_paths_without_trusting_primary() {
        let primary = tempfile::tempdir().expect("primary");
        let secondary = tempfile::tempdir().expect("secondary");
        std::fs::write(primary.path().join("primary.rs"), "primary").expect("primary fixture");
        std::fs::write(secondary.path().join("secondary.rs"), "secondary")
            .expect("secondary fixture");
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([primary.path(), secondary.path()])
            .with_trusted_read_roots([secondary.path()]);
        let no_prompt = CountingDeny(AtomicUsize::new(0));
        let secondary_read = PermissionRequest {
            id: "secondary-read".to_owned(),
            tool_name: "read".to_owned(),
            arguments: json!({"path": "@root/1/secondary.rs"}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(secondary_read, &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn untrusted_nested_root_does_not_inherit_primary_read_authority() {
        let tree = tempfile::tempdir().expect("tree");
        let nested = tree.path().join("nested");
        std::fs::create_dir(&nested).expect("nested root");
        std::fs::write(nested.join("private.rs"), "private").expect("nested fixture");
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([tree.path(), nested.as_path()])
            .with_trusted_read_roots([tree.path()]);
        let prompt = CountingDeny(AtomicUsize::new(0));
        let request = PermissionRequest {
            id: "nested-read".to_owned(),
            tool_name: "read".to_owned(),
            arguments: json!({"path": "@root/1/private.rs"}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(request, &prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(prompt.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn remembered_glob_approval_applies_without_reprompt_and_survives_reload() {
        let root = tempfile::tempdir().expect("tempdir");
        let approval_file = root.path().join("approvals.json");
        let request = PermissionRequest {
            id: "glob-first".to_owned(),
            tool_name: "glob".to_owned(),
            arguments: json!({"pattern": "**/*.rs", "path": "."}),
            capabilities: vec![ToolCapability::ReadFilesystem],
            approval_diff: None,
        };
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&approval_file);
        assert_eq!(
            gate.authorize(request.clone(), &Decision(ApprovalDecision::AllowProject),)
                .await,
            PermissionOutcome::Allowed
        );
        let no_prompt = CountingDeny(AtomicUsize::new(0));
        let mut repeated = request.clone();
        repeated.id = "glob-second".to_owned();
        assert_eq!(
            gate.authorize(repeated.clone(), &no_prompt).await,
            PermissionOutcome::Allowed
        );
        let reloaded = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&approval_file);
        assert_eq!(
            reloaded.authorize(repeated, &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn approval_listing_is_opaque_and_revocation_updates_private_persistence() {
        let root = tempfile::tempdir().expect("tempdir");
        let approval_file = root.path().join("approvals.json");
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&approval_file);
        let session_request = request(
            "printf SECRET_SESSION_CANARY",
            vec![ToolCapability::Execute],
        );
        let project_request = request(
            "printf SECRET_PROJECT_CANARY",
            vec![ToolCapability::Execute],
        );
        let hidden_fingerprint =
            PermissionKey::from_request(&project_request, &workspace_namespace([root.path()]))
                .arguments_fingerprint;
        assert_eq!(
            gate.authorize(
                session_request.clone(),
                &Decision(ApprovalDecision::AllowSession),
            )
            .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(
                project_request.clone(),
                &Decision(ApprovalDecision::AllowProject),
            )
            .await,
            PermissionOutcome::Allowed
        );
        let approvals = gate.approval_snapshot();
        assert_eq!(approvals.session.len(), 1);
        assert_eq!(approvals.project.len(), 1);
        let rendered = serde_json::to_string(&approvals).expect("approval snapshot");
        assert!(!rendered.contains("SECRET_SESSION_CANARY"));
        assert!(!rendered.contains("SECRET_PROJECT_CANARY"));
        assert!(!rendered.contains("arguments_fingerprint"));
        assert!(!rendered.contains(&hidden_fingerprint));
        assert_eq!(
            approvals.project[0].canonical_summary,
            "exact-invocation=hidden capabilities=Execute approval=none"
        );
        assert!(
            !std::fs::read_to_string(&approval_file)
                .expect("private approvals")
                .contains("SECRET_PROJECT_CANARY")
        );
        let stable = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&approval_file)
            .approval_snapshot();
        assert_eq!(stable.project[0].id, approvals.project[0].id);

        assert_eq!(
            gate.revoke_session_approvals(Some(&approvals.session[0].id)),
            1
        );
        assert_eq!(
            gate.revoke_project_approvals(Some(&approvals.project[0].id))
                .expect("persist project revocation"),
            1
        );
        assert!(gate.approval_snapshot().session.is_empty());
        assert!(gate.approval_snapshot().project.is_empty());
        let deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(session_request, &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.authorize(project_request, &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 2);

        let reloaded = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&approval_file);
        assert_eq!(reloaded.snapshot().project_approvals, 0);
    }

    #[test]
    fn independent_project_stores_reload_merge_and_never_resurrect_revoked_authority() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("approvals.json");
        let first = independent_project_store(&path);
        let second = independent_project_store(&path);
        let namespace = workspace_namespace([root.path()]);
        let key_a = PermissionKey::from_request(
            &request("printf authority-a", vec![ToolCapability::Execute]),
            &namespace,
        );
        let key_b = PermissionKey::from_request(
            &request("printf authority-b", vec![ToolCapability::Execute]),
            &namespace,
        );
        first.grant(key_a.clone()).expect("grant A");
        let stale = second.refresh().expect("stale gate load");
        let id_a = stale
            .iter()
            .find(|entry| entry.key == key_a)
            .expect("A")
            .id
            .clone();
        assert_eq!(first.revoke(Some(&id_a)).expect("revoke A"), 1);
        assert!(!second.contains(&key_a).expect("fresh deny in gate2"));

        second.grant(key_b.clone()).expect("stale gate grants B");
        let authoritative = first.refresh().expect("authoritative reload");
        assert!(!contains_approval(&authoritative, &key_a));
        assert!(contains_approval(&authoritative, &key_b));
        assert_eq!(authoritative.len(), 1);
        let stable_id = authoritative.iter().next().expect("B").id.clone();
        let reloaded = independent_project_store(&path)
            .refresh()
            .expect("reload stable id");
        assert_eq!(reloaded.iter().next().expect("reloaded B").id, stable_id);
    }

    #[tokio::test]
    async fn project_revocation_in_one_gate_immediately_denies_another_gate() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("approvals.json");
        let first = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&path);
        let second = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&path);
        let authority_a = request("printf authority-a", vec![ToolCapability::Execute]);
        let authority_b = request("printf authority-b", vec![ToolCapability::Execute]);
        assert_eq!(
            first
                .authorize(
                    authority_a.clone(),
                    &Decision(ApprovalDecision::AllowProject),
                )
                .await,
            PermissionOutcome::Allowed
        );
        let deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            second.authorize(authority_a.clone(), &deny).await,
            PermissionOutcome::Allowed
        );
        let id_a = first.approval_snapshot().project[0].id.clone();
        assert_eq!(
            first
                .revoke_project_approvals(Some(&id_a))
                .expect("revoke A"),
            1
        );
        assert_eq!(
            second.authorize(authority_a.clone(), &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            second
                .authorize(
                    authority_b.clone(),
                    &Decision(ApprovalDecision::AllowProject)
                )
                .await,
            PermissionOutcome::Allowed
        );
        let authoritative = first.approval_snapshot();
        assert_eq!(authoritative.project.len(), 1);
        assert_eq!(authoritative.project[0].tool_name, "bash");
        assert_eq!(
            first.authorize(authority_a, &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            first.authorize(authority_b, &deny).await,
            PermissionOutcome::Allowed
        );
    }

    #[test]
    fn concurrent_independent_project_grant_and_revoke_serialize_without_lost_updates() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("approvals.json");
        let first = Arc::new(independent_project_store(&path));
        let second = Arc::new(independent_project_store(&path));
        let namespace = workspace_namespace([root.path()]);
        let key_a = PermissionKey::from_request(
            &request("printf authority-a", vec![ToolCapability::Execute]),
            &namespace,
        );
        let key_b = PermissionKey::from_request(
            &request("printf authority-b", vec![ToolCapability::Execute]),
            &namespace,
        );
        first.grant(key_a.clone()).expect("grant A");
        let id_a = first
            .refresh()
            .expect("load A")
            .iter()
            .next()
            .expect("A")
            .id
            .clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let first = Arc::clone(&first);
            let first_barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                first_barrier.wait();
                first.revoke(Some(&id_a)).expect("concurrent revoke A");
            });
            let second = Arc::clone(&second);
            let second_barrier = Arc::clone(&barrier);
            let key_b = key_b.clone();
            scope.spawn(move || {
                second_barrier.wait();
                second.grant(key_b).expect("concurrent grant B");
            });
        });
        let authoritative = independent_project_store(&path)
            .refresh()
            .expect("authoritative reload");
        assert!(!contains_approval(&authoritative, &key_a));
        assert!(contains_approval(&authoritative, &key_b));
        assert_eq!(authoritative.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn durable_project_write_is_private_unique_and_cleans_failed_rename() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join("approvals.json");
        fs::create_dir(&target).expect("rename-blocking directory");
        let namespace = workspace_namespace([root.path()]);
        let key = PermissionKey::from_request(
            &request("printf durable", vec![ToolCapability::Execute]),
            &namespace,
        );
        let approval = RememberedApproval::new("project", key).expect("random id");
        let approvals = BTreeSet::from([approval]);
        assert!(persist_project_approvals(&target, &approvals).is_err());
        let prefix = format!(
            "{}.",
            target.file_name().expect("target name").to_string_lossy()
        );
        assert!(
            fs::read_dir(root.path())
                .expect("root entries")
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(&prefix))
        );

        fs::remove_dir(&target).expect("remove blocker");
        persist_project_approvals(&target, &approvals).expect("durable write");
        assert_eq!(
            fs::metadata(&target)
                .expect("ledger metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(load_project_approvals(&target).expect("reload"), approvals);
    }

    #[cfg(unix)]
    #[test]
    fn malformed_project_ledger_is_not_overwritten_by_a_stale_grant() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("approvals.json");
        fs::write(&path, b"{malformed").expect("malformed ledger");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private ledger");
        let namespace = workspace_namespace([root.path()]);
        let key = PermissionKey::from_request(
            &request("printf stale", vec![ToolCapability::Execute]),
            &namespace,
        );
        let store = independent_project_store(&path);
        assert!(store.grant(key).is_err());
        assert_eq!(
            fs::read(&path).expect("unchanged malformed ledger"),
            b"{malformed"
        );
    }

    #[tokio::test]
    async fn clear_session_removes_rules_and_allow_session_approvals() {
        let gate = PermissionGate::new(PermissionDecision::Ask);
        gate.add_session_rule(PermissionRule {
            pattern: "bash(cargo test*)".to_owned(),
            action: PermissionDecision::Allow,
        })
        .expect("session rule");
        assert_eq!(
            gate.authorize(
                request("printf remember", vec![ToolCapability::Execute]),
                &Decision(ApprovalDecision::AllowSession),
            )
            .await,
            PermissionOutcome::Allowed
        );
        let cleared = gate.clear_session_permissions();
        assert_eq!(cleared.rules, 1);
        assert_eq!(cleared.approvals, 1);
        assert!(gate.snapshot().session_rules.is_empty());
        assert_eq!(gate.snapshot().session_approvals, 0);
    }

    #[tokio::test]
    async fn workspace_generation_swap_invalidates_old_session_and_project_approvals() {
        let root = tempfile::tempdir().expect("tempdir");
        let added = root.path().join("added");
        std::fs::create_dir(&added).expect("added root");
        let approval_file = root.path().join("approvals.json");
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&approval_file);
        let session_request = request("printf session", vec![ToolCapability::Execute]);
        let project_request = request("printf project", vec![ToolCapability::Execute]);
        assert_eq!(
            gate.authorize(
                session_request.clone(),
                &Decision(ApprovalDecision::AllowSession),
            )
            .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(
                project_request.clone(),
                &Decision(ApprovalDecision::AllowProject),
            )
            .await,
            PermissionOutcome::Allowed
        );
        let update = gate
            .replace_workspace_roots([root.path(), added.as_path()])
            .expect("workspace generation swap");
        assert_eq!(update.generation, 2);
        assert_eq!(update.invalidated_session_approvals, 1);
        assert_eq!(update.invalidated_project_approvals, 1);
        assert_eq!(gate.snapshot().session_approvals, 0);
        assert_eq!(gate.snapshot().project_approvals, 0);

        let deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(session_request, &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.authorize(project_request, &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 2);
        let unchanged = gate
            .replace_workspace_roots([root.path(), added.as_path()])
            .expect("same generation");
        assert_eq!(unchanged.generation, 2);
        assert_eq!(unchanged.invalidated_session_approvals, 0);
        assert_eq!(unchanged.invalidated_project_approvals, 0);
    }

    #[tokio::test]
    async fn workspace_gate_fork_preserves_rules_without_inheriting_session_authority() {
        let root = tempfile::tempdir().expect("tempdir");
        let added = root.path().join("added");
        fs::create_dir(&added).expect("added root");
        let approval_file = root.path().join("approvals.json");
        let original = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(&approval_file);
        original
            .add_session_rule(PermissionRule {
                pattern: "bash(cargo test*)".to_owned(),
                action: PermissionDecision::Allow,
            })
            .expect("session rule");
        let remembered = request("printf remembered", vec![ToolCapability::Execute]);
        assert_eq!(
            original
                .authorize(
                    remembered.clone(),
                    &Decision(ApprovalDecision::AllowSession),
                )
                .await,
            PermissionOutcome::Allowed
        );
        let project = bash_request("/bin/echo project", root.path());
        assert_eq!(
            original
                .authorize(project.clone(), &Decision(ApprovalDecision::AllowProject),)
                .await,
            PermissionOutcome::Allowed
        );
        let persisted_before = fs::read(&approval_file).expect("project ledger");
        let replacement = original
            .fork_for_workspace_roots([root.path(), added.as_path()])
            .expect("replacement gate");
        assert_eq!(
            replacement.snapshot().session_rules,
            original.snapshot().session_rules
        );
        assert_eq!(replacement.snapshot().session_approvals, 0);
        assert_eq!(replacement.snapshot().project_approvals, 0);
        assert_eq!(
            fs::read(&approval_file).expect("unchanged ledger"),
            persisted_before
        );
        let deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            replacement.authorize(remembered.clone(), &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            original.authorize(remembered, &deny).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            replacement.authorize(project.clone(), &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            original.authorize(project, &deny).await,
            PermissionOutcome::Allowed
        );
    }

    #[tokio::test]
    async fn command_allow_rule_cannot_silently_add_network_authority() {
        let command_rule = PermissionRule {
            pattern: "bash(cargo test*)".to_owned(),
            action: PermissionDecision::Allow,
        };
        let invocation = |network| PermissionRequest {
            id: "network-call".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({
                "command": "cargo test",
                "network_domains": if network { vec!["example.com"] } else { Vec::new() },
            }),
            capabilities: vec![ToolCapability::Execute, ToolCapability::Network],
            approval_diff: None,
        };
        let gate = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![command_rule.clone()],
        });
        let deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(invocation(false), &deny).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            gate.authorize(invocation(true), &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 1);

        let network_gate = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![
                command_rule,
                PermissionRule {
                    pattern: "network(bash)".to_owned(),
                    action: PermissionDecision::Allow,
                },
            ],
        });
        assert_eq!(
            network_gate.authorize(invocation(true), &deny).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn user_safe_list_is_zero_prompt_only_for_sandboxed_networkless_commands() {
        let safety = Arc::new(
            CommandSafetyClassifier::new(&["cargo test*".to_owned()]).expect("user safe-list"),
        );
        let gate = PermissionGate::new(PermissionDecision::Ask).with_command_safety(safety);
        let request = |command: &str, sandbox: &str, domains: Vec<&str>| PermissionRequest {
            id: "safe-list-call".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({
                "command": command,
                "sandbox": sandbox,
                "network_domains": domains,
            }),
            capabilities: vec![ToolCapability::Execute, ToolCapability::Network],
            approval_diff: None,
        };

        let deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(request("cargo test", "sandboxed", vec![]), &deny)
                .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            gate.authorize(
                request("cargo test && rm -rf target", "sandboxed", vec![]),
                &deny,
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.authorize(
                request("cargo test", "sandboxed", vec!["example.com"]),
                &deny,
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.authorize(request("cargo test", "unsandboxed", vec![]), &deny)
                .await,
            PermissionOutcome::Denied
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn unsandboxed_escape_hatch_requires_explicit_and_exact_authority() {
        let root = tempfile::tempdir().expect("root");
        let unsandboxed = PermissionRequest {
            id: "unsandboxed-call".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({
                "command": "/bin/echo canary",
                "cwd": root.path(),
                "env": {},
                "network_domains": [],
                "sandbox": "unsandboxed",
            }),
            capabilities: vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
            approval_diff: None,
        };
        let generic_allow = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![PermissionRule {
                pattern: "bash(echo*)".to_owned(),
                action: PermissionDecision::Allow,
            }],
        });
        let prompted = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            generic_allow
                .authorize(unsandboxed.clone(), &prompted)
                .await,
            PermissionOutcome::Denied
        );
        assert_eq!(prompted.0.load(Ordering::SeqCst), 1);

        let gate = PermissionGate::new(PermissionDecision::Ask).with_workspace_roots([root.path()]);
        assert_eq!(
            gate.authorize(
                unsandboxed.clone(),
                &Decision(ApprovalDecision::AllowSession),
            )
            .await,
            PermissionOutcome::Allowed
        );
        let no_prompt = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(unsandboxed.clone(), &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);

        let mut sandboxed = unsandboxed.clone();
        sandboxed.arguments["sandbox"] = Value::String("sandboxed".to_owned());
        assert_eq!(
            gate.authorize(sandboxed, &no_prompt).await,
            PermissionOutcome::Denied
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 1);

        let mode_deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            PermissionGate::new(PermissionDecision::Ask)
                .authorize_in_mode(unsandboxed.clone(), &mode_deny, None, SessionMode::Plan,)
                .await,
            PermissionOutcome::Denied
        );
        assert_eq!(mode_deny.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            PermissionGate::for_headless_mode(HeadlessPermissionMode::AutoSafe)
                .authorize(unsandboxed.clone(), &mode_deny)
                .await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            PermissionGate::for_headless_mode(HeadlessPermissionMode::Yolo)
                .authorize(unsandboxed, &mode_deny)
                .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(mode_deny.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auto_safe_allows_only_reversible_workspace_file_tools() {
        let fixture = tempfile::tempdir().expect("fixture");
        let primary = fixture.path().join("primary");
        let added = fixture.path().join("added");
        let outside = fixture.path().join("outside");
        for root in [&primary, &added, &outside] {
            fs::create_dir(root).expect("workspace fixture");
        }
        let gate = PermissionGate::for_headless_mode(HeadlessPermissionMode::AutoSafe)
            .with_workspace_roots([&primary, &added]);
        let approver = CountingDeny(AtomicUsize::new(0));
        let write = |path: &str| PermissionRequest {
            id: "auto-safe-write".to_owned(),
            tool_name: "write".to_owned(),
            arguments: json!({"path": path, "content": "fixture"}),
            capabilities: vec![
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
            ],
            approval_diff: None,
        };

        assert_eq!(
            gate.authorize(write("new.txt"), &approver).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(write("@root/1/new.txt"), &approver).await,
            PermissionOutcome::Allowed
        );
        let multi_edit = |path: &str| PermissionRequest {
            id: "auto-safe-multi-edit".to_owned(),
            tool_name: "multi_edit".to_owned(),
            arguments: json!({
                "path": path,
                "edits": [{"old": "before", "new": "after"}],
            }),
            capabilities: vec![
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
            ],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(multi_edit("@root/1/existing.txt"), &approver)
                .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(
                multi_edit(outside.join("existing.txt").to_str().expect("UTF-8")),
                &approver,
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.authorize(
                write(outside.join("escaped.txt").to_str().expect("UTF-8")),
                &approver
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.authorize(write("../outside/escaped.txt"), &approver)
                .await,
            PermissionOutcome::Denied
        );

        let mut network_write = write("network.txt");
        network_write.capabilities.push(ToolCapability::Network);
        assert_eq!(
            gate.authorize(network_write, &approver).await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_safe_does_not_follow_workspace_symlinks_for_write_approval() {
        let fixture = tempfile::tempdir().expect("fixture");
        let workspace = fixture.path().join("workspace");
        let outside = fixture.path().join("outside");
        fs::create_dir(&workspace).expect("workspace");
        fs::create_dir(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, workspace.join("escape")).expect("symlink");
        let gate = PermissionGate::for_headless_mode(HeadlessPermissionMode::AutoSafe)
            .with_workspace_roots([&workspace]);
        let request = PermissionRequest {
            id: "symlink-write".to_owned(),
            tool_name: "edit".to_owned(),
            arguments: json!({"path": "escape/file.txt", "old": "a", "new": "b"}),
            capabilities: vec![
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
            ],
            approval_diff: None,
        };
        let approver = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(request, &approver).await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn explicitly_typed_unsandboxed_patterns_are_rememberable_without_generic_escalation() {
        let root = tempfile::tempdir().expect("root");
        let request = |command: &str| PermissionRequest {
            id: "unsandboxed-pattern".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({
                "command": command,
                "cwd": root.path(),
                "env": {},
                "network_domains": [],
                "sandbox": "unsandboxed",
            }),
            capabilities: vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
            approval_diff: None,
        };
        let explicit_rule = PermissionRule {
            pattern: "bash_unsandboxed(echo *)".to_owned(),
            action: PermissionDecision::Allow,
        };
        let gate = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![explicit_rule.clone()],
        });
        let no_prompt = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(request("/bin/echo first"), &no_prompt).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            gate.authorize(request("/bin/printf first"), &no_prompt)
                .await,
            PermissionOutcome::Denied
        );

        let denied = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![
                explicit_rule.clone(),
                PermissionRule {
                    pattern: "bash(echo*)".to_owned(),
                    action: PermissionDecision::Deny,
                },
            ],
        });
        assert_eq!(
            denied
                .authorize(request("/bin/echo first"), &no_prompt)
                .await,
            PermissionOutcome::Denied
        );

        let strict = PermissionGate::for_headless_mode(HeadlessPermissionMode::Strict);
        strict
            .add_session_rule(explicit_rule.clone())
            .expect("typed session rule");
        assert_eq!(
            strict
                .authorize(request("/bin/echo session"), &no_prompt)
                .await,
            PermissionOutcome::Allowed
        );
        let auto_safe = PermissionGate::for_headless_mode(HeadlessPermissionMode::AutoSafe);
        auto_safe
            .add_session_rule(explicit_rule)
            .expect("typed session rule");
        assert_eq!(
            auto_safe
                .authorize(request("/bin/echo session"), &no_prompt)
                .await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.authorize_in_mode(
                request("/bin/echo plan"),
                &no_prompt,
                None,
                SessionMode::Plan,
            )
            .await,
            PermissionOutcome::Denied
        );
    }

    #[tokio::test]
    async fn built_in_git_status_safe_list_binds_bare_git_and_rejects_workspace_paths() {
        let gate = PermissionGate::new(PermissionDecision::Ask);
        let approver = CountingDeny(AtomicUsize::new(0));
        let capabilities = vec![ToolCapability::ReadFilesystem, ToolCapability::Execute];
        assert_eq!(
            gate.authorize_in_mode(
                request("git status --short", capabilities.clone()),
                &approver,
                None,
                SessionMode::Execute,
            )
            .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            gate.authorize_in_mode(
                request("./git status", capabilities.clone()),
                &approver,
                None,
                SessionMode::Execute,
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            gate.authorize_in_mode(
                request("git status && printf unsafe", capabilities),
                &approver,
                None,
                SessionMode::Execute,
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 2);

        let denied = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![PermissionRule {
                pattern: "bash(git status*)".to_owned(),
                action: PermissionDecision::Deny,
            }],
        });
        assert_eq!(
            denied
                .authorize_in_mode(
                    request("git status", vec![ToolCapability::ReadFilesystem]),
                    &approver,
                    None,
                    SessionMode::Execute,
                )
                .await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 2);
        assert_eq!(
            gate.authorize_in_mode(
                request("git status", vec![ToolCapability::ReadFilesystem]),
                &approver,
                Some(PermissionOutcome::Denied),
                SessionMode::Execute,
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malicious_workspace_git_is_never_executed_or_exposed_by_safe_list() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("workspace");
        let marker = root.path().join("malicious-git-executed");
        let executable = root.path().join("git");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf HOST_SECRET_CANARY\ntouch '{}'\n",
                marker.display()
            ),
        )
        .expect("malicious git fixture");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("malicious git mode");

        let outcome = PermissionGate::new(PermissionDecision::Ask)
            .authorize_in_mode(
                request(
                    "./git status",
                    vec![ToolCapability::ReadFilesystem, ToolCapability::Execute],
                ),
                &CountingDeny(AtomicUsize::new(0)),
                None,
                SessionMode::Execute,
            )
            .await;
        let output = if outcome == PermissionOutcome::Allowed {
            std::process::Command::new("./git")
                .arg("status")
                .current_dir(root.path())
                .output()
                .expect("malicious git execution")
                .stdout
        } else {
            Vec::new()
        };
        assert_eq!(outcome, PermissionOutcome::Denied);
        assert!(!marker.exists(), "workspace-controlled git was executed");
        assert!(!String::from_utf8_lossy(&output).contains("HOST_SECRET_CANARY"));
    }

    #[tokio::test]
    async fn plan_and_discuss_deny_mutation_even_under_yolo() {
        let gate = PermissionGate::for_headless_mode(HeadlessPermissionMode::Yolo);
        for mode in [SessionMode::Plan, SessionMode::Discuss] {
            assert_eq!(
                gate.authorize_in_mode(
                    request(
                        "rm -rf build",
                        vec![ToolCapability::Execute, ToolCapability::WriteFilesystem]
                    ),
                    &Decision(ApprovalDecision::AllowOnce),
                    Some(PermissionOutcome::Allowed),
                    mode,
                )
                .await,
                PermissionOutcome::Denied
            );
        }
    }

    #[tokio::test]
    async fn default_policy_prompts_only_for_file_writes_and_unsafe_bash() {
        let gate = PermissionGate::new(PermissionDecision::Ask);
        let approver = CountingDeny(AtomicUsize::new(0));
        for request in [
            PermissionRequest {
                id: "read".to_owned(),
                tool_name: "read".to_owned(),
                arguments: json!({"path": "README.md"}),
                capabilities: vec![ToolCapability::ReadFilesystem],
                approval_diff: None,
            },
            PermissionRequest {
                id: "todo".to_owned(),
                tool_name: "todo".to_owned(),
                arguments: json!({"action": "clear"}),
                capabilities: Vec::new(),
                approval_diff: None,
            },
            PermissionRequest {
                id: "webfetch".to_owned(),
                tool_name: "webfetch".to_owned(),
                arguments: json!({"url": "https://example.com/"}),
                capabilities: vec![ToolCapability::Network],
                approval_diff: None,
            },
            PermissionRequest {
                id: "mcp".to_owned(),
                tool_name: "mcp__fixture__inspect".to_owned(),
                arguments: json!({}),
                capabilities: vec![ToolCapability::Network, ToolCapability::Execute],
                approval_diff: None,
            },
        ] {
            assert_eq!(
                gate.authorize(request, &approver).await,
                PermissionOutcome::Allowed
            );
        }
        assert_eq!(approver.0.load(Ordering::SeqCst), 0);

        let write = PermissionRequest {
            id: "write".to_owned(),
            tool_name: "write".to_owned(),
            arguments: json!({"path": "README.md", "content": "changed"}),
            capabilities: vec![
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
            ],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(write, &approver).await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.authorize(
                request(
                    "/bin/echo unsafe",
                    vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
                ),
                &approver,
            )
            .await,
            PermissionOutcome::Denied
        );
        assert_eq!(approver.0.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn runtime_yolo_is_session_local_reversible_and_never_weakens_explicit_denies() {
        let gate = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![
                PermissionRule {
                    pattern: "write(denied.txt)".to_owned(),
                    action: PermissionDecision::Deny,
                },
                PermissionRule {
                    pattern: "write(asked.txt)".to_owned(),
                    action: PermissionDecision::Ask,
                },
            ],
        });
        let deny = CountingDeny(AtomicUsize::new(0));
        let write = |path: &str| PermissionRequest {
            id: format!("write-{path}"),
            tool_name: "write".to_owned(),
            arguments: json!({"path": path, "content": "fixture"}),
            capabilities: vec![ToolCapability::WriteFilesystem],
            approval_diff: None,
        };

        assert_eq!(
            gate.authorize(write("allowed.txt"), &deny).await,
            PermissionOutcome::Denied
        );
        gate.set_runtime_mode(Some(HeadlessPermissionMode::Yolo))
            .expect("interactive yolo");
        assert_eq!(
            gate.authorize(write("allowed.txt"), &deny).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(write("asked.txt"), &deny).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(write("denied.txt"), &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(
            gate.snapshot().runtime_mode,
            Some(HeadlessPermissionMode::Yolo)
        );
        gate.set_runtime_mode(None)
            .expect("restore configured policy");
        assert_eq!(
            gate.authorize(write("allowed.txt"), &deny).await,
            PermissionOutcome::Denied
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 2);

        let fixed = PermissionGate::for_headless_mode(HeadlessPermissionMode::Strict);
        assert!(
            fixed
                .set_runtime_mode(Some(HeadlessPermissionMode::Yolo))
                .is_err(),
            "remote/headless strict policy must not be client-switchable"
        );
        assert!(root_yolo_footgun(true, &[PathBuf::from("/")]));
        assert!(!root_yolo_footgun(false, &[PathBuf::from("/")]));
        assert!(!root_yolo_footgun(true, &[PathBuf::from("/tmp/project")]));
    }

    #[tokio::test]
    async fn runtime_yolo_survives_child_workspace_forks_and_never_prompts_for_subagent_control() {
        let parent = tempfile::tempdir().expect("parent workspace");
        let child = tempfile::tempdir().expect("child workspace");
        let gate = PermissionGate::from_config(PermissionConfig {
            default: PermissionDecision::Ask,
            rules: Vec::new(),
        })
        .with_workspace_roots([parent.path()]);
        let child_gate = gate
            .fork_for_workspace_roots([child.path()])
            .expect("child permission generation");
        gate.set_runtime_mode(Some(HeadlessPermissionMode::Yolo))
            .expect("interactive yolo");
        assert_eq!(
            child_gate.snapshot().runtime_mode,
            Some(HeadlessPermissionMode::Yolo),
            "an existing child must observe later parent permission-mode changes"
        );
        gate.add_session_rule(PermissionRule {
            pattern: "bash(cargo test*)".to_owned(),
            action: PermissionDecision::Allow,
        })
        .expect("parent session rule");
        assert_eq!(
            child_gate.snapshot().session_rules,
            gate.snapshot().session_rules,
            "an existing child must observe later parent session-rule changes"
        );

        let approver = CountingDeny(AtomicUsize::new(0));
        let spawn = PermissionRequest {
            id: "spawn-general".to_owned(),
            tool_name: "spawn_agent".to_owned(),
            arguments: json!({
                "action": "spawn",
                "task": "inspect and update the delegated workspace",
                "agent": "general",
                "isolation": "shared",
            }),
            capabilities: Vec::new(),
            approval_diff: None,
        };
        assert_eq!(
            child_gate.authorize(spawn, &approver).await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            approver.0.load(Ordering::SeqCst),
            0,
            "YOLO subagent control must not enter the interactive approval channel"
        );
    }

    #[tokio::test]
    async fn project_approval_round_trips_privately() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("approvals.json");
        let gate = PermissionGate::new(PermissionDecision::Ask).with_project_approval_file(&path);
        let invocation = request("git status", vec![ToolCapability::ReadFilesystem]);
        assert_eq!(
            gate.authorize(
                invocation.clone(),
                &Decision(ApprovalDecision::AllowProject)
            )
            .await,
            PermissionOutcome::Allowed
        );
        let recovered =
            PermissionGate::new(PermissionDecision::Ask).with_project_approval_file(&path);
        assert_eq!(
            recovered
                .authorize(invocation, &Decision(ApprovalDecision::Deny))
                .await,
            PermissionOutcome::Allowed
        );
    }

    #[tokio::test]
    async fn remembered_mutations_bind_full_arguments_diff_and_bash_execution_context() {
        let gate = PermissionGate::new(PermissionDecision::Ask);
        let write = PermissionRequest {
            id: "write".to_owned(),
            tool_name: "write".to_owned(),
            arguments: json!({"path": "same.txt", "content": "approved"}),
            capabilities: vec![ToolCapability::WriteFilesystem],
            approval_diff: Some(UnifiedDiff {
                proposal_id: "proposal".to_owned(),
                path: "same.txt".to_owned(),
                unified_diff: "diff".to_owned(),
                arguments_hash: "args".to_owned(),
                base_hash: "base-a".to_owned(),
                diff_hash: "diff-a".to_owned(),
                truncated: false,
            }),
        };
        assert_eq!(
            gate.authorize(write.clone(), &Decision(ApprovalDecision::AllowSession))
                .await,
            PermissionOutcome::Allowed
        );
        let deny = CountingDeny(AtomicUsize::new(0));
        let mut same_proposal = write.clone();
        same_proposal
            .approval_diff
            .as_mut()
            .expect("approval diff")
            .proposal_id = "different-call-instance".to_owned();
        assert_eq!(
            gate.authorize(same_proposal, &deny).await,
            PermissionOutcome::Allowed
        );
        let mut changed_content = write.clone();
        changed_content.arguments = json!({"path": "same.txt", "content": "different"});
        assert_eq!(
            gate.authorize(changed_content, &deny).await,
            PermissionOutcome::Denied
        );
        let mut changed_base = write;
        changed_base
            .approval_diff
            .as_mut()
            .expect("approval diff")
            .base_hash = "base-b".to_owned();
        assert_eq!(
            gate.authorize(changed_base, &deny).await,
            PermissionOutcome::Denied
        );

        let bash_gate = PermissionGate::new(PermissionDecision::Ask);
        let bash = PermissionRequest {
            id: "bash".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({
                "command": "/bin/echo test",
                "cwd": "crate-a",
                "env": {"PATH": "/trusted/bin", "GIT_CONFIG_COUNT": "0"},
                "network_domains": []
            }),
            capabilities: vec![ToolCapability::Execute],
            approval_diff: None,
        };
        assert_eq!(
            bash_gate
                .authorize(bash.clone(), &Decision(ApprovalDecision::AllowSession))
                .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            bash_gate.authorize(bash.clone(), &deny).await,
            PermissionOutcome::Allowed
        );
        for arguments in [
            json!({"command": "/bin/echo test", "cwd": "crate-b", "env": {"PATH": "/trusted/bin"}, "network_domains": []}),
            json!({"command": "/bin/echo test", "cwd": "crate-a", "env": {"PATH": "/attacker/bin"}, "network_domains": []}),
        ] {
            let mut changed = bash.clone();
            changed.arguments = arguments;
            assert_eq!(
                bash_gate.authorize(changed, &deny).await,
                PermissionOutcome::Denied
            );
        }
        assert_eq!(deny.0.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn project_approvals_bind_ordered_complete_workspace_roots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let replacement = temp.path().join("replacement");
        for root in [&first, &second, &replacement] {
            fs::create_dir(root).expect("workspace root");
        }
        let approvals = temp.path().join("approvals.json");
        let invocation = request("/bin/echo test", vec![ToolCapability::Execute]);
        let initial = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([&first, &second])
            .with_project_approval_file(&approvals);
        assert_eq!(
            initial
                .authorize(
                    invocation.clone(),
                    &Decision(ApprovalDecision::AllowProject)
                )
                .await,
            PermissionOutcome::Allowed
        );
        for roots in [[&second, &first], [&first, &replacement]] {
            let reloaded = PermissionGate::new(PermissionDecision::Ask)
                .with_workspace_roots(roots)
                .with_project_approval_file(&approvals);
            assert_eq!(
                reloaded
                    .authorize(invocation.clone(), &Decision(ApprovalDecision::Deny))
                    .await,
                PermissionOutcome::Denied
            );
        }
    }

    #[tokio::test]
    async fn remembered_network_domains_are_normalized_exact_and_invalid_fail_closed() {
        let gate = PermissionGate::new(PermissionDecision::Ask);
        let invocation = |domains: Vec<&str>| PermissionRequest {
            id: "network-domains".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({
                "command": "/bin/echo network",
                "cwd": ".",
                "env": {},
                "network_domains": domains,
            }),
            capabilities: vec![ToolCapability::Execute, ToolCapability::Network],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(
                invocation(vec!["Example.COM.", "api.example.com"]),
                &Decision(ApprovalDecision::AllowSession),
            )
            .await,
            PermissionOutcome::Allowed
        );
        let deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(
                invocation(vec!["api.example.com", "example.com", "EXAMPLE.COM"]),
                &deny,
            )
            .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            gate.authorize(invocation(vec!["other.example.com"]), &deny)
                .await,
            PermissionOutcome::Denied
        );
        let yolo = PermissionGate::for_headless_mode(HeadlessPermissionMode::Yolo);
        assert_eq!(
            yolo.authorize(invocation(vec!["https://invalid.example"]), &deny)
                .await,
            PermissionOutcome::Denied
        );
    }

    #[tokio::test]
    async fn webfetch_is_no_prompt_for_every_valid_public_origin() {
        let gate = PermissionGate::new(PermissionDecision::Ask);
        let request = |url: &str| PermissionRequest {
            id: "webfetch".to_owned(),
            tool_name: "webfetch".to_owned(),
            arguments: json!({"url": url, "headers": {}}),
            capabilities: vec![ToolCapability::Network],
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(
                request("https://Example.com/path/a?query=one"),
                &Decision(ApprovalDecision::AllowSession),
            )
            .await,
            PermissionOutcome::Allowed
        );
        let deny = CountingDeny(AtomicUsize::new(0));
        assert_eq!(
            gate.authorize(request("https://example.com/other/path"), &deny)
                .await,
            PermissionOutcome::Allowed
        );
        for url in [
            "https://sub.example.com/path/a",
            "https://example.com:8443/path/a",
            "http://example.com/path/a",
        ] {
            assert_eq!(
                gate.authorize(request(url), &deny).await,
                PermissionOutcome::Allowed
            );
        }
        assert_eq!(
            gate.authorize(request("file:///private/etc/passwd"), &deny)
                .await,
            PermissionOutcome::Denied
        );
        assert_eq!(deny.0.load(Ordering::SeqCst), 0);
    }
}
