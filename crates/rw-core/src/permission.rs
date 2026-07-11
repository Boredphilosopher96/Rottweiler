use std::{
    collections::BTreeSet,
    fmt, fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use async_trait::async_trait;
use globset::GlobBuilder;
use rw_tools::{CommandSafety, classify_safe_command};
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
    pub rules: Vec<PermissionRule>,
    pub session_rules: Vec<PermissionRule>,
    pub session_approvals: usize,
    pub project_approvals: usize,
}

/// Single mandatory permission chokepoint with mode overlays, pattern rules,
/// and exact invocation approvals remembered at session or project scope.
pub struct PermissionGate {
    policy: PermissionPolicy,
    workspace_namespace: Vec<String>,
    session_rules: RwLock<Vec<PermissionRule>>,
    session_allows: RwLock<BTreeSet<PermissionKey>>,
    project_allows: RwLock<BTreeSet<PermissionKey>>,
    project_file: Option<PathBuf>,
}

impl fmt::Debug for PermissionGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionGate")
            .field("policy", &self.policy)
            .field("snapshot", &self.snapshot())
            .field("project_persistence", &self.project_file.is_some())
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
            workspace_namespace: Vec::new(),
            session_rules: RwLock::new(Vec::new()),
            session_allows: RwLock::new(BTreeSet::new()),
            project_allows: RwLock::new(BTreeSet::new()),
            project_file: None,
        }
    }

    /// Enables durable exact-invocation approvals. Unsafe or malformed files
    /// fail closed by loading no approvals; writes remain atomic and private.
    #[must_use]
    pub fn with_project_approval_file(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let loaded = load_project_approvals(&path).unwrap_or_default();
        self.project_allows = RwLock::new(loaded);
        self.project_file = Some(path);
        self
    }

    #[must_use]
    pub fn for_headless_mode(mode: HeadlessPermissionMode) -> Self {
        Self {
            policy: PermissionPolicy::Headless(mode),
            workspace_namespace: Vec::new(),
            session_rules: RwLock::new(Vec::new()),
            session_allows: RwLock::new(BTreeSet::new()),
            project_allows: RwLock::new(BTreeSet::new()),
            project_file: None,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> PermissionSnapshot {
        let (default, rules) = match &self.policy {
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
        PermissionSnapshot {
            default,
            rules,
            session_rules: lock_read(&self.session_rules).clone(),
            session_approvals: lock_read(&self.session_allows).len(),
            project_approvals: lock_read(&self.project_allows).len(),
        }
    }

    /// Binds remembered approvals to the complete ordered root identity.
    #[must_use]
    pub fn with_workspace_roots(
        mut self,
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Self {
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
        self.workspace_namespace = vec![namespace.finalize().to_hex().to_string()];
        self
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
        if mode != SessionMode::Execute && !is_mode_read_only(&request) {
            return PermissionOutcome::Denied;
        }
        if ask_override == Some(PermissionOutcome::Denied) {
            return PermissionOutcome::Denied;
        }
        let key = PermissionKey::from_request(&request, &self.workspace_namespace);
        match self.decision_for(&request) {
            PermissionDecision::Allow => PermissionOutcome::Allowed,
            PermissionDecision::Deny => PermissionOutcome::Denied,
            PermissionDecision::Ask => {
                if let Some(outcome) = ask_override {
                    return outcome;
                }
                if lock_read(&self.session_allows).contains(&key)
                    || lock_read(&self.project_allows).contains(&key)
                {
                    return PermissionOutcome::Allowed;
                }
                match approver.decide(request).await {
                    ApprovalDecision::AllowOnce => PermissionOutcome::Allowed,
                    ApprovalDecision::AllowSession => {
                        lock_write(&self.session_allows).insert(key);
                        PermissionOutcome::Allowed
                    }
                    ApprovalDecision::AllowProject => {
                        let mut approvals = lock_write(&self.project_allows);
                        approvals.insert(key.clone());
                        if self
                            .project_file
                            .as_ref()
                            .is_some_and(|path| persist_project_approvals(path, &approvals).is_ok())
                        {
                            PermissionOutcome::Allowed
                        } else {
                            approvals.remove(&key);
                            PermissionOutcome::Denied
                        }
                    }
                    ApprovalDecision::Deny => PermissionOutcome::Denied,
                }
            }
        }
    }

    fn decision_for(&self, request: &PermissionRequest) -> PermissionDecision {
        if matches!(request.tool_name.as_str(), "ask_user" | "submit_plan")
            && request.capabilities.is_empty()
        {
            return PermissionDecision::Allow;
        }
        let safe_listed = request.tool_name == "bash"
            && request
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| classify_safe_command(command) == CommandSafety::SafeListed);
        match &self.policy {
            PermissionPolicy::Configured(config) => {
                let mut effective = config.clone();
                effective
                    .rules
                    .extend(lock_read(&self.session_rules).iter().cloned());
                let configured = rule_decision(&effective, request);
                if configured == PermissionDecision::Ask && safe_listed {
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
                if rules.is_empty() {
                    match mode {
                        HeadlessPermissionMode::Strict if safe_listed => PermissionDecision::Allow,
                        HeadlessPermissionMode::AutoSafe
                            if safe_listed || is_read_only(request) =>
                        {
                            PermissionDecision::Allow
                        }
                        _ => default,
                    }
                } else {
                    rule_decision(&PermissionConfig { default, rules }, request)
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
enum PermissionPolicy {
    Configured(PermissionConfig),
    Headless(HeadlessPermissionMode),
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct PermissionKey {
    tool_name: String,
    canonical_arguments: String,
    capabilities: Vec<String>,
    approval_fingerprint: Option<String>,
    workspace_namespace: Vec<String>,
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
            canonical_arguments: canonical_key_arguments(request),
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
        && let Some(commands) = canonical_shell_commands(command)
        && let Some(object) = arguments.as_object_mut()
    {
        object.insert(
            "command".to_owned(),
            Value::Array(commands.into_iter().map(Value::String).collect()),
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

fn is_mode_read_only(request: &PermissionRequest) -> bool {
    is_read_only(request)
        || (request.tool_name == "bash"
            && request
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| classify_safe_command(command) == CommandSafety::SafeListed))
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
    if command.contains('`') || command.contains("$(") {
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
        if command_index > 0 {
            argv.drain(..command_index);
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

fn load_project_approvals(path: &Path) -> Result<BTreeSet<PermissionKey>, std::io::Error> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Ok(BTreeSet::new());
        }
    }
    serde_json::from_slice(&fs::read(path)?).or(Ok(BTreeSet::new()))
}

fn persist_project_approvals(
    path: &Path,
    approvals: &BTreeSet<PermissionKey>,
) -> Result<(), std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(
        &temporary,
        serde_json::to_vec(approvals).unwrap_or_default(),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(temporary, path)
}

fn lock_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
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

    fn request(command: &str, capabilities: Vec<ToolCapability>) -> PermissionRequest {
        PermissionRequest {
            id: "call".to_owned(),
            tool_name: "bash".to_owned(),
            arguments: json!({ "command": command }),
            capabilities,
            approval_diff: None,
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
                request("git status && cat README", read),
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
                "command": "cargo test",
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
            json!({"command": "cargo test", "cwd": "crate-b", "env": {"PATH": "/trusted/bin"}, "network_domains": []}),
            json!({"command": "cargo test", "cwd": "crate-a", "env": {"PATH": "/attacker/bin"}, "network_domains": []}),
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
        let invocation = request("cargo test", vec![ToolCapability::Execute]);
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
                "command": "cargo test",
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
    async fn webfetch_remembrance_is_same_origin_not_exact_path() {
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
                PermissionOutcome::Denied
            );
        }
        assert_eq!(deny.0.load(Ordering::SeqCst), 3);
    }
}
