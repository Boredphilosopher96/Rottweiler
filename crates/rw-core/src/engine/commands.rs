mod navigation;
#[allow(clippy::wildcard_imports)]
use super::*;

/// Immutable descriptor-only view retaining its registered metadata sources.
pub type SessionCommandCatalog =
    rw_ext::CommandCatalog<SessionCommandContext, SessionCommandOutput>;

/// Engine-owned slash-command context. Public handlers use this exact type.
#[derive(Clone, Debug)]
pub struct SessionCommandContext {
    pub(super) session_id: SessionId,
    pub(super) running: bool,
    pub(super) queued_messages: usize,
    pub(super) mode: SessionMode,
    pub(super) mode_id: ModeId,
    pub(super) modes: Arc<ModeRegistry>,
    pub(super) permission_summary: String,
    pub(super) command_summary: String,
}

impl Default for SessionCommandContext {
    fn default() -> Self {
        Self {
            session_id: SessionId("command-fixture".to_owned()),
            running: false,
            queued_messages: 0,
            mode: SessionMode::Execute,
            mode_id: ModeId("execute".to_owned()),
            modes: Arc::new(ModeRegistry::builtins().unwrap_or_default()),
            permission_summary: String::new(),
            command_summary: String::new(),
        }
    }
}

impl SessionCommandContext {
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[must_use]
    pub const fn running(&self) -> bool {
        self.running
    }

    #[must_use]
    pub const fn queued_messages(&self) -> usize {
        self.queued_messages
    }

    #[must_use]
    pub const fn mode(&self) -> SessionMode {
        self.mode
    }

    #[must_use]
    pub fn mode_id(&self) -> &ModeId {
        &self.mode_id
    }
}

/// Command result interpreted by the actor after common registry dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCommandOutput {
    pub message: String,
    pub action: SessionCommandAction,
}

/// Typed tool work that must complete through the ordinary engine pipeline
/// before a custom-command prompt is committed or submitted to a model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandToolCall {
    pub placeholder: String,
    pub name: String,
    pub arguments: Value,
    pub output_kind: CommandToolOutputKind,
}

/// Untrusted framing applied when a command-tool result replaces its opaque
/// prompt placeholder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandToolOutputKind {
    FileInclusion { path: String },
    ShellInterpolation,
    StructuredToolResult { source: String },
}

/// Actor action requested by a command handler.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SessionCommandAction {
    #[default]
    None,
    Navigate {
        target: rw_types::extension_control::SessionNavigationTarget,
    },
    Rewind {
        to_turn: u64,
    },
    Review,
    Context,
    PinContext {
        item_id: ContextItemId,
    },
    EvictContext {
        item_id: ContextItemId,
    },
    Cost,
    Compact {
        instructions: Option<String>,
    },
    SwitchMode {
        mode: ModeId,
    },
    SetPermissionMode {
        mode: Option<rw_types::PermissionModeDescriptor>,
    },
    AddPermissionRule {
        rule: PermissionRule,
    },
    RemovePermissionRule {
        pattern: String,
    },
    ClearSessionPermissions,
    ListPermissionApprovals,
    RevokeSessionApprovals {
        id: Option<String>,
    },
    RevokeProjectApprovals {
        id: Option<String>,
    },
    Trust {
        operation: FolderTrustOperation,
    },
    AddWorkspaceRoot {
        path: PathBuf,
    },
    /// Runs bounded repository analysis and checkpointed AGENTS.md creation.
    InitializeWorkspace {
        depth: InitDepth,
    },
    /// Starts a normal model turn from an expanded declarative command.
    /// `allowed_tools` narrows the turn's tool set when present;
    /// `pre_approvals` are `tool(glob)` invocations that run without an
    /// approval prompt during the turn.
    SubmitPrompt {
        content: String,
        model_alias: Option<String>,
        allowed_tools: Option<Vec<String>>,
        pre_approvals: Vec<String>,
        tool_calls: Vec<CommandToolCall>,
    },
}

/// Explicit folder-trust ledger operation requested by `/trust`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FolderTrustOperation {
    Status,
    Grant { confirmation: Option<String> },
    Revoke,
}

/// Host-injected folder-trust boundary. Core never reads a platform ledger directly.
#[async_trait]
pub trait FolderTrustController: Send + Sync {
    async fn execute(&self, operation: FolderTrustOperation) -> Result<String, AgentLoopError>;
}

/// Folder-trust boundary for embedded/test actors that do not configure persistence.
#[derive(Debug, Default)]
pub struct NoopFolderTrustController;

#[async_trait]
impl FolderTrustController for NoopFolderTrustController {
    async fn execute(&self, _operation: FolderTrustOperation) -> Result<String, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "folder trust is unavailable for this session host".to_owned(),
        ))
    }
}

/// Complete immutable runtime boundary swapped after a live root append.
pub struct WorkspaceRuntimeGeneration {
    pub model: Arc<dyn super::ModelDriver>,
    pub publication: super::RuntimePublication,
    pub ui: Arc<dyn crate::ui::UiRegistry>,
    pub generation: u64,
    pub effective_from_turn: u64,
    pub roots: Vec<PathBuf>,
    pub tools: Arc<ToolRegistry>,
    pub hooks: Arc<HookDispatcher>,
    pub commands: Arc<CommandRegistry<SessionCommandContext, SessionCommandOutput>>,
    pub modes: Arc<ModeRegistry>,
    pub permissions: Arc<PermissionGate>,
    pub checkpoints: Arc<dyn MutationCheckpointCoordinator>,
    pub folder_trust: Arc<dyn FolderTrustController>,
    pub supplemental_context: super::session::InitialSessionContext,
}

impl fmt::Debug for WorkspaceRuntimeGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceRuntimeGeneration")
            .field("generation", &self.generation)
            .field("effective_from_turn", &self.effective_from_turn)
            .field("roots", &self.roots)
            .field("supplemental_context", &self.supplemental_context)
            .finish_non_exhaustive()
    }
}

/// Captured actor authority for one append-only root preparation.
pub struct WorkspaceRootRequest<'a> {
    pub requested: &'a Path,
    pub roots: &'a [PathBuf],
    pub generation: u64,
    pub effective_from_turn: u64,
    pub permissions: Arc<PermissionGate>,
    pub model: Arc<dyn super::ModelDriver>,
    pub model_alias: &'a str,
    pub mcp_policy: rw_tools::McpToolPolicy,
}

/// Host-owned builder and persistence boundary for live workspace generations.
#[async_trait]
pub trait WorkspaceRootController: Send + Sync {
    async fn append_root(
        &self,
        request: WorkspaceRootRequest<'_>,
    ) -> Result<WorkspaceRuntimeGeneration, AgentLoopError>;

    async fn prepare_commit_generation(&self, generation: u64) -> Result<(), AgentLoopError>;

    /// Finalizes already-prepared in-memory boundaries after the durable root
    /// event is committed. This phase must be infallible.
    fn finalize_generation(&self, generation: u64);

    async fn abort_generation(&self, _generation: u64) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct NoopWorkspaceRootController;

#[async_trait]
impl WorkspaceRootController for NoopWorkspaceRootController {
    async fn append_root(
        &self,
        _request: WorkspaceRootRequest<'_>,
    ) -> Result<WorkspaceRuntimeGeneration, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "live workspace-root changes are unavailable for this session host".to_owned(),
        ))
    }

    async fn prepare_commit_generation(&self, _generation: u64) -> Result<(), AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "live workspace-root changes are unavailable for this session host".to_owned(),
        ))
    }

    fn finalize_generation(&self, _generation: u64) {}

    async fn abort_generation(&self, _generation: u64) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

fn permission_decision_label(decision: PermissionDecision) -> &'static str {
    decision.as_str()
}

pub(super) fn render_permission_snapshot(
    snapshot: &crate::permission::PermissionSnapshot,
) -> String {
    let mode = snapshot
        .runtime_mode
        .map_or("standard", rw_types::PermissionModeDescriptor::as_str);
    let mut lines = vec![
        format!("Permission mode: {mode}"),
        format!(
            "Default permission: {}",
            permission_decision_label(snapshot.default)
        ),
    ];
    if snapshot.rules.is_empty() {
        lines.push("Configured rules: none".to_owned());
    } else {
        lines.push("Configured rules:".to_owned());
        lines.extend(snapshot.rules.iter().map(|rule| {
            format!(
                "- {} · {}",
                permission_decision_label(rule.action),
                rule.pattern
            )
        }));
    }
    for (title, rules) in [
        ("Project rules", &snapshot.project_rules),
        ("Session rules", &snapshot.session_rules),
    ] {
        if rules.is_empty() {
            lines.push(format!("{title}: none"));
        } else {
            lines.push(format!("{title}:"));
            lines.extend(rules.iter().map(|rule| {
                format!(
                    "- {} · {}",
                    permission_decision_label(rule.action),
                    rule.pattern
                )
            }));
        }
    }
    lines.push(format!(
        "Remembered approvals: {} for this session, {} for this project",
        snapshot.session_approvals, snapshot.project_approvals
    ));
    lines.join("\n")
}

pub(super) fn render_permission_approvals(
    snapshot: &crate::permission::PermissionApprovalSnapshot,
) -> String {
    if snapshot.session.is_empty() && snapshot.project.is_empty() {
        return "Remembered approvals: none".to_owned();
    }
    let mut lines = Vec::new();
    for (title, approvals) in [
        ("This session", &snapshot.session),
        ("This project", &snapshot.project),
    ] {
        if approvals.is_empty() {
            continue;
        }
        lines.push(format!("{title}:"));
        lines.extend(approvals.iter().map(|approval| {
            format!(
                "- {} · {} · revoke with {}",
                approval.tool_name, approval.canonical_summary, approval.id
            )
        }));
    }
    lines.join("\n")
}

fn review_status_label(status: ReviewFileStatus) -> &'static str {
    match status {
        ReviewFileStatus::Pending => "needs review",
        ReviewFileStatus::Accepted => "accepted",
        ReviewFileStatus::Reverted => "reverted",
    }
}

pub(super) fn render_session_review(review: &SessionReview) -> String {
    if review.files.is_empty() {
        return "Session review: no changed files".to_owned();
    }
    let pending = review
        .files
        .iter()
        .filter(|file| file.status == ReviewFileStatus::Pending)
        .count();
    let mut lines = vec![format!(
        "Session review: {} changed file(s) · {pending} awaiting review",
        review.files.len()
    )];
    lines.extend(review.files.iter().take(50).map(|file| {
        let detail = if file.unrestorable_reason.is_some() {
            " · cannot be restored automatically"
        } else if file.truncated {
            " · diff truncated"
        } else {
            ""
        };
        format!(
            "- {} · {}{detail}",
            file.path,
            review_status_label(file.status)
        )
    }));
    if review.files.len() > 50 {
        lines.push(format!("…and {} more", review.files.len() - 50));
    }
    lines.join("\n")
}

fn context_item_kind_label(kind: &ContextItemKind) -> &'static str {
    match kind {
        ContextItemKind::System => "System",
        ContextItemKind::ToolDefinitions => "Tools",
        ContextItemKind::ProjectInstructions => "Project instructions",
        ContextItemKind::Conversation => "Conversation",
        ContextItemKind::ToolResult => "Tool result",
        ContextItemKind::Pinned => "Pinned context",
        ContextItemKind::QueuedMessage => "Queued message",
    }
}

pub(super) fn render_context_snapshot(snapshot: &ContextSnapshot) -> String {
    let utilization = if snapshot.context_window_known && snapshot.usable_tokens > 0 {
        format!(
            " ({}%)",
            snapshot.used_tokens.saturating_mul(100) / snapshot.usable_tokens
        )
    } else {
        String::new()
    };
    let capacity = if snapshot.context_window_known {
        snapshot.usable_tokens.to_string()
    } else {
        "unknown".to_owned()
    };
    let mut lines = vec![format!(
        "Context: {} of {capacity} usable tokens{utilization}",
        snapshot.used_tokens
    )];
    if snapshot.reserved_tokens > 0 {
        lines.push(format!(
            "Reserved for the response: {} tokens",
            snapshot.reserved_tokens
        ));
    }
    if !snapshot.context_window_known
        && let Some(reason) = snapshot.context_window_reason.as_deref()
    {
        lines.push(format!("Capacity unavailable: {reason}"));
    }
    lines.push(format!("Context items: {}", snapshot.items.len()));
    for item in snapshot.items.iter().take(20) {
        let mut state = Vec::new();
        if item.state.pinned {
            state.push("pinned");
        }
        if item.state.evicted {
            state.push("evicted");
        }
        if item.state.summarized {
            state.push("summarized");
        }
        if item.state.pruned {
            state.push("pruned");
        }
        let suffix = if state.is_empty() {
            String::new()
        } else {
            format!(" · {}", state.join(", "))
        };
        lines.push(format!(
            "- {} · {} · {} tokens{suffix}",
            context_item_kind_label(&item.kind),
            item.label,
            item.estimated_tokens
        ));
    }
    if snapshot.items.len() > 20 {
        lines.push(format!("…and {} more", snapshot.items.len() - 20));
    }
    lines.join("\n")
}

pub(super) fn render_cost_snapshot(snapshot: &CostSnapshot) -> String {
    let usage = &snapshot.session_usage;
    let mut lines = vec![format!(
        "Session usage: {} input · {} output · {} reasoning tokens",
        usage.input_tokens, usage.output_tokens, usage.reasoning_tokens
    )];
    if usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0 {
        lines.push(format!(
            "Cache: {} read · {} written tokens · {}% hit rate",
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            snapshot.cache_hit_basis_points / 100
        ));
    }
    if snapshot.session_monetary_accounting_complete {
        lines.push(format!(
            "Session cost: ${}.{:06}",
            snapshot.session_cost_micros_usd / 1_000_000,
            snapshot.session_cost_micros_usd % 1_000_000
        ));
    } else if snapshot.session_subscription_quota_entries > 0 {
        lines.push("Session cost: covered by subscription quota".to_owned());
    } else {
        lines.push("Session cost: unavailable".to_owned());
    }
    if snapshot.session_ai_credit_micros > 0 {
        lines.push(format!(
            "AI credits used: {}.{:06}",
            snapshot.session_ai_credit_micros / 1_000_000,
            snapshot.session_ai_credit_micros % 1_000_000
        ));
    }
    lines.join("\n")
}

struct HelpCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for HelpCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        _invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: context.command_summary.clone(),
            action: SessionCommandAction::None,
        })
    }
}

struct ModeCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for ModeCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let value = invocation.arguments().trim();
        if value.is_empty() {
            let available = context
                .modes
                .iter()
                .map(|mode| {
                    let marker = if mode.id() == &context.mode_id {
                        "*"
                    } else {
                        "-"
                    };
                    format!("{marker} {} — {}", mode.id().0, mode.description())
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(SessionCommandOutput {
                message: format!(
                    "Active mode: {}\nAvailable modes:\n{available}",
                    context.mode_id.0
                ),
                action: SessionCommandAction::None,
            });
        }
        if context.modes.get(value).is_none() {
            let available = context
                .modes
                .iter()
                .map(|mode| mode.id().0.as_str())
                .collect::<Vec<_>>()
                .join("|");
            return Err(CommandExecutionError::new(
                "invalid_mode",
                format!("usage: /mode [{available}]"),
            ));
        }
        Ok(SessionCommandOutput {
            message: format!("mode changed to {value}"),
            action: SessionCommandAction::SwitchMode {
                mode: ModeId(value.to_owned()),
            },
        })
    }
}

struct PermissionsCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for PermissionsCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let arguments = invocation.arguments().trim();
        if arguments.is_empty() || arguments == "list" {
            return Ok(SessionCommandOutput {
                message: context.permission_summary.clone(),
                action: SessionCommandAction::None,
            });
        }
        if arguments == "clear-session" {
            return Ok(SessionCommandOutput {
                message: String::new(),
                action: SessionCommandAction::ClearSessionPermissions,
            });
        }
        if arguments == "approvals" {
            return Ok(SessionCommandOutput {
                message: String::new(),
                action: SessionCommandAction::ListPermissionApprovals,
            });
        }
        if arguments == "trust" || arguments.starts_with("trust ") {
            return Ok(SessionCommandOutput {
                message: String::new(),
                action: SessionCommandAction::Trust {
                    operation: folder_trust_operation(&arguments["trust".len()..])?,
                },
            });
        }
        if let Some(value) = arguments.strip_prefix("mode ").map(str::trim) {
            let mode = if matches!(value, "default" | "standard") {
                None
            } else {
                Some(value.parse().map_err(|_| invalid_permissions_command())?)
            };
            return Ok(SessionCommandOutput {
                message: String::new(),
                action: SessionCommandAction::SetPermissionMode { mode },
            });
        }
        for (prefix, project) in [("revoke-session ", false), ("revoke-project ", true)] {
            if let Some(value) = arguments.strip_prefix(prefix).map(str::trim) {
                if value.is_empty() {
                    return Err(invalid_permissions_command());
                }
                let id = (value != "all").then(|| value.to_owned());
                return Ok(SessionCommandOutput {
                    message: String::new(),
                    action: if project {
                        SessionCommandAction::RevokeProjectApprovals { id }
                    } else {
                        SessionCommandAction::RevokeSessionApprovals { id }
                    },
                });
            }
        }
        if let Some(pattern) = arguments.strip_prefix("remove ").map(str::trim) {
            if pattern.is_empty() {
                return Err(invalid_permissions_command());
            }
            return Ok(SessionCommandOutput {
                message: String::new(),
                action: SessionCommandAction::RemovePermissionRule {
                    pattern: pattern.to_owned(),
                },
            });
        }
        if let Some(addition) = arguments.strip_prefix("add ") {
            let Some((decision, pattern)) = addition.trim().split_once(char::is_whitespace) else {
                return Err(invalid_permissions_command());
            };
            let action = match decision {
                "allow" => PermissionDecision::Allow,
                "ask" => PermissionDecision::Ask,
                "deny" => PermissionDecision::Deny,
                _ => return Err(invalid_permissions_command()),
            };
            let pattern = pattern.trim();
            if pattern.is_empty() {
                return Err(invalid_permissions_command());
            }
            return Ok(SessionCommandOutput {
                message: String::new(),
                action: SessionCommandAction::AddPermissionRule {
                    rule: PermissionRule {
                        pattern: pattern.to_owned(),
                        action,
                    },
                },
            });
        }
        Err(invalid_permissions_command())
    }
}

fn invalid_permissions_command() -> CommandExecutionError {
    CommandExecutionError::new(
        "invalid_permissions_command",
        "usage: /permissions [list | mode <default|strict|auto-safe|yolo> | approvals | add <allow|ask|deny> <tool(glob)> | remove <tool(glob)> | clear-session | revoke-session <id|all> | revoke-project <id|all> | trust [status|grant|revoke]]",
    )
}

fn folder_trust_operation(arguments: &str) -> Result<FolderTrustOperation, CommandExecutionError> {
    match arguments.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(FolderTrustOperation::Status),
        ["grant"] => Ok(FolderTrustOperation::Grant { confirmation: None }),
        ["grant", confirmation] => Ok(FolderTrustOperation::Grant {
            confirmation: Some((*confirmation).to_owned()),
        }),
        ["revoke"] => Ok(FolderTrustOperation::Revoke),
        _ => Err(CommandExecutionError::new(
            "invalid_trust_command",
            "usage: /permissions trust [status|grant|revoke]",
        )),
    }
}

struct ContextCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for ContextCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let arguments = invocation.arguments().trim();
        let action = if arguments.is_empty() {
            SessionCommandAction::Context
        } else {
            if context.running() {
                return Err(CommandExecutionError::new(
                    "turn_running",
                    "context surgery requires an idle session",
                ));
            }
            let Some((operation, item_id)) = arguments.split_once(char::is_whitespace) else {
                return Err(CommandExecutionError::new(
                    "invalid_context_command",
                    "usage: /context [pin <item-id> | evict <item-id>]",
                ));
            };
            let item_id = item_id.trim();
            if item_id.is_empty() {
                return Err(CommandExecutionError::new(
                    "invalid_context_command",
                    "usage: /context [pin <item-id> | evict <item-id>]",
                ));
            }
            match operation {
                "pin" => SessionCommandAction::PinContext {
                    item_id: ContextItemId(item_id.to_owned()),
                },
                "evict" => SessionCommandAction::EvictContext {
                    item_id: ContextItemId(item_id.to_owned()),
                },
                _ => {
                    return Err(CommandExecutionError::new(
                        "invalid_context_command",
                        "usage: /context [pin <item-id> | evict <item-id>]",
                    ));
                }
            }
        };
        Ok(SessionCommandOutput {
            message: String::new(),
            action,
        })
    }
}

struct UsageCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for UsageCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        if !invocation.arguments().trim().is_empty() {
            return Err(CommandExecutionError::new(
                "invalid_usage_command",
                "usage: /usage",
            ));
        }
        Ok(SessionCommandOutput {
            message: String::new(),
            action: SessionCommandAction::Cost,
        })
    }
}

struct CompactCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for CompactCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let instructions = invocation.arguments().trim();
        Ok(SessionCommandOutput {
            message: "compaction started".to_owned(),
            action: SessionCommandAction::Compact {
                instructions: (!instructions.is_empty()).then(|| instructions.to_owned()),
            },
        })
    }
}

struct RewindCommand;

struct ReviewCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for ReviewCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        if context.running() {
            return Err(CommandExecutionError::new(
                "turn_running",
                "session review requires an idle session",
            ));
        }
        if !invocation.arguments().trim().is_empty() {
            return Err(CommandExecutionError::new(
                "invalid_review_command",
                "usage: /review",
            ));
        }
        Ok(SessionCommandOutput {
            message: String::new(),
            action: SessionCommandAction::Review,
        })
    }
}

struct DirsCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for DirsCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let path = invocation.arguments().trim();
        if path.is_empty() {
            return Err(CommandExecutionError::new(
                "interactive_client_required",
                "Listing workspace roots requires an interactive client; add one with /dirs <path>.",
            ));
        }
        if context.running() {
            return Err(CommandExecutionError::new(
                "turn_running",
                "adding a workspace root requires an idle session",
            ));
        }
        Ok(SessionCommandOutput {
            message: String::new(),
            action: SessionCommandAction::AddWorkspaceRoot {
                path: PathBuf::from(path),
            },
        })
    }
}

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for RewindCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        if context.running() {
            return Err(CommandExecutionError::new(
                "turn_running",
                "interrupt the active turn before rewinding",
            ));
        }
        let to_turn = invocation.arguments().parse::<u64>().map_err(|_| {
            CommandExecutionError::new(
                "invalid_turn",
                "usage: /rewind <turn>; interactive clients open the turn timeline",
            )
        })?;
        Ok(SessionCommandOutput {
            message: format!("rewound to turn {to_turn}"),
            action: SessionCommandAction::Rewind { to_turn },
        })
    }
}

/// Registers every core-owned catalog entry through rw-ext's public registry API.
///
/// Runtime-owned entries (`mcp`, `init`, `memory`) are registered by the host
/// that owns their backing resources, using the same catalog metadata.
///
/// # Errors
///
/// Returns an extension error if any built-in registration is invalid.
pub fn builtin_command_registry()
-> Result<CommandRegistry<SessionCommandContext, SessionCommandOutput>, AgentLoopError> {
    let mut registry = CommandRegistry::new();
    for entry in rw_types::client_navigation::COMMAND_CATALOG {
        if entry.registrar != rw_types::client_navigation::CommandRegistrar::Core {
            continue;
        }
        let handler: Arc<dyn CommandHandler<SessionCommandContext, SessionCommandOutput>> =
            match entry.name {
                "rewind" => Arc::new(RewindCommand),
                "compact" => Arc::new(CompactCommand),
                "mode" => Arc::new(ModeCommand),
                "context" => Arc::new(ContextCommand),
                "usage" => Arc::new(UsageCommand),
                "review" => Arc::new(ReviewCommand),
                "dirs" => Arc::new(DirsCommand),
                "permissions" => Arc::new(PermissionsCommand),
                "help" => Arc::new(HelpCommand),
                _ => Arc::new(navigation::InteractiveClientCommand),
            };
        registry
            .register_shared(CommandDescriptor::from_catalog(entry), handler)
            .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    }
    Ok(registry)
}
