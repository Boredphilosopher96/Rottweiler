#![cfg(test)]

use crate::InitDepth;
use crate::PermissionApprover;
use crate::PermissionGate;
use crate::PermissionRequest;
use crate::engine::AgentLoopError;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::commands::CommandToolCall;
use crate::engine::commands::CommandToolOutputKind;
use crate::engine::commands::FolderTrustController;
use crate::engine::commands::FolderTrustOperation;
use crate::engine::commands::NoopFolderTrustController;
use crate::engine::commands::SessionCommandAction;
use crate::engine::commands::SessionCommandContext;
use crate::engine::commands::SessionCommandOutput;
use crate::engine::commands::WorkspaceRootController;
use crate::engine::commands::WorkspaceRuntimeGeneration;
use crate::engine::commands::builtin_command_registry;
use crate::engine::mutation_checkpoints::NoopMutationCheckpointCoordinator;
use crate::engine::session_extension::SessionExtensionController;
use crate::engine::session_extension::SessionExtensionSnapshot;
use crate::engine::tests::fixtures::support::descriptor;
use async_trait::async_trait;
use rw_ext::CommandDescriptor;
use rw_ext::CommandExecutionError;
use rw_ext::CommandHandler;
use rw_ext::CommandInvocation;
use rw_ext::ModeRegistry;
use rw_tools::AskUserInput;
use rw_tools::CancellationToken;
use rw_tools::QuestionAsker;
use rw_tools::Tool;
use rw_tools::ToolContext;
use rw_tools::ToolDescriptor;
use rw_tools::ToolError;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::ApprovalDecision;
use rw_types::SessionId;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

pub(in crate::engine::tests) struct StaticApprover(pub(in crate::engine::tests) ApprovalDecision);

#[async_trait]
impl PermissionApprover for StaticApprover {
    async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
        self.0.clone()
    }
}

pub(in crate::engine::tests) struct EchoCommand;

pub(in crate::engine::tests) struct ScopedPromptCommand;

pub(in crate::engine::tests) struct PreludePromptCommand {
    pub(in crate::engine::tests) command: String,
}

pub(in crate::engine::tests) struct InitActionCommand(pub(in crate::engine::tests) InitDepth);

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for InitActionCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        _invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: "workspace initialization started".to_owned(),
            action: SessionCommandAction::InitializeWorkspace { depth: self.0 },
        })
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct RecordingFolderTrust {
    pub(in crate::engine::tests) operations: Mutex<Vec<FolderTrustOperation>>,
}

pub(in crate::engine::tests) struct FixedWorkspaceRootController {
    pub(in crate::engine::tests) extensions: Option<Arc<FixedSessionExtensionController>>,
    pub(in crate::engine::tests) roots: Vec<PathBuf>,
    pub(in crate::engine::tests) tools: Arc<ToolRegistry>,
    pub(in crate::engine::tests) permissions: Arc<PermissionGate>,
    pub(in crate::engine::tests) committed: AtomicU64,
    pub(in crate::engine::tests) aborted: AtomicU64,
    pub(in crate::engine::tests) fail_commit: bool,
}

#[derive(Default)]
pub(in crate::engine::tests) struct FixedSessionExtensionController {
    pub(in crate::engine::tests) base: Mutex<Option<SessionExtensionSnapshot>>,
    pub(in crate::engine::tests) reject: AtomicBool,
    pub(in crate::engine::tests) attaches: AtomicUsize,
    pub(in crate::engine::tests) detaches: AtomicUsize,
}

#[async_trait]
impl SessionExtensionController for FixedSessionExtensionController {
    async fn attach(
        &self,
        _source: &Path,
        current: SessionExtensionSnapshot,
    ) -> Result<SessionExtensionSnapshot, AgentLoopError> {
        self.attaches.fetch_add(1, Ordering::SeqCst);
        if self.reject.load(Ordering::SeqCst) {
            return Err(AgentLoopError::InvalidConfiguration(
                "seeded development reload rejection".to_owned(),
            ));
        }
        let base = {
            let mut stored = self.base.lock().expect("extension base");
            stored.get_or_insert(current).clone()
        };
        let mut commands = base.commands.as_ref().clone();
        commands
            .register(
                CommandDescriptor::new(
                    "development-marker",
                    "command from the active development plugin generation",
                ),
                EchoCommand,
            )
            .expect("development marker command");
        Ok(SessionExtensionSnapshot {
            publication: crate::RuntimePublication::Active,
            model: base.model,
            model_alias: base.model_alias,
            ui: Arc::new(crate::ui::EmptyUiRegistry),
            revision: base.revision.saturating_add(1),
            workspace_roots: base.workspace_roots,
            tools: base.tools,
            hooks: base.hooks,
            commands: Arc::new(commands),
        })
    }

    async fn detach(
        &self,
        _current: SessionExtensionSnapshot,
    ) -> Result<SessionExtensionSnapshot, AgentLoopError> {
        self.detaches.fetch_add(1, Ordering::SeqCst);
        self.base
            .lock()
            .expect("extension base")
            .take()
            .ok_or_else(|| {
                AgentLoopError::InvalidConfiguration(
                    "no development generation is active".to_owned(),
                )
            })
    }

    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

impl FixedSessionExtensionController {
    pub(in crate::engine::tests) fn rebase(
        &self,
        current: SessionExtensionSnapshot,
    ) -> (SessionExtensionSnapshot, bool) {
        let mut stored = self.base.lock().expect("extension base");
        if stored.is_none() {
            return (current, false);
        }
        *stored = Some(current.clone());
        let mut commands = current.commands.as_ref().clone();
        commands
            .register(
                CommandDescriptor::new(
                    "development-marker",
                    "command from the active development plugin generation",
                ),
                EchoCommand,
            )
            .expect("development marker command");
        (
            SessionExtensionSnapshot {
                publication: crate::RuntimePublication::Active,
                model: current.model,
                model_alias: current.model_alias,
                ui: Arc::new(crate::ui::EmptyUiRegistry),
                revision: current.revision.saturating_add(1),
                workspace_roots: current.workspace_roots,
                tools: current.tools,
                hooks: current.hooks,
                commands: Arc::new(commands),
            },
            false,
        )
    }
}

#[async_trait]
impl WorkspaceRootController for FixedWorkspaceRootController {
    async fn append_root(
        &self,
        request: crate::WorkspaceRootRequest<'_>,
    ) -> Result<WorkspaceRuntimeGeneration, AgentLoopError> {
        let mut commands = builtin_command_registry().expect("generation commands");
        commands
            .register(
                CommandDescriptor::new(
                    "generation-marker",
                    "command discovered from the new workspace generation",
                ),
                EchoCommand,
            )
            .expect("generation marker command");
        let mut generation = WorkspaceRuntimeGeneration {
            model: request.model,
            publication: crate::RuntimePublication::Active,
            ui: Arc::new(crate::ui::EmptyUiRegistry),
            generation: request.generation + 1,
            effective_from_turn: request.effective_from_turn,
            roots: self.roots.clone(),
            tools: Arc::clone(&self.tools),
            hooks: Arc::new(builtin_hook_dispatcher().expect("generation hooks")),
            commands: Arc::new(commands),
            modes: Arc::new(ModeRegistry::builtins().expect("generation modes")),
            permissions: Arc::clone(&self.permissions),
            checkpoints: Arc::new(NoopMutationCheckpointCoordinator),
            folder_trust: Arc::new(NoopFolderTrustController),
            supplemental_context: Vec::new(),
        };
        if let Some(extensions) = &self.extensions {
            let (snapshot, _) = extensions.rebase(SessionExtensionSnapshot {
                publication: generation.publication.clone(),
                model: generation.model.clone(),
                model_alias: request.model_alias.to_owned(),
                ui: generation.ui.clone(),
                revision: generation.generation,
                workspace_roots: Arc::from(generation.roots.clone()),
                tools: generation.tools.clone(),
                hooks: generation.hooks.clone(),
                commands: generation.commands.clone(),
            });
            generation.tools = snapshot.tools;
            generation.hooks = snapshot.hooks;
            generation.commands = snapshot.commands;
        }
        Ok(generation)
    }

    async fn prepare_commit_generation(&self, _generation: u64) -> Result<(), AgentLoopError> {
        if self.fail_commit {
            return Err(AgentLoopError::Persistence(
                "fixture marker commit failed".to_owned(),
            ));
        }
        Ok(())
    }

    fn finalize_generation(&self, generation: u64) {
        self.committed.store(generation, Ordering::SeqCst);
    }

    async fn abort_generation(&self, generation: u64) -> Result<(), AgentLoopError> {
        self.aborted.store(generation, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl FolderTrustController for RecordingFolderTrust {
    async fn execute(&self, operation: FolderTrustOperation) -> Result<String, AgentLoopError> {
        let message = format!("trust operation: {operation:?}");
        self.operations
            .lock()
            .expect("trust operations")
            .push(operation);
        Ok(message)
    }
}

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for EchoCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: invocation.arguments().to_owned(),
            action: SessionCommandAction::None,
        })
    }
}

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for ScopedPromptCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        _invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: "scoped prompt started".to_owned(),
            action: SessionCommandAction::SubmitPrompt {
                content: "scoped prompt".to_owned(),
                model_alias: Some("slow".to_owned()),
                allowed_tools: Some(vec!["read".to_owned()]),
                permission_patterns: Vec::new(),
                tool_calls: Vec::new(),
            },
        })
    }
}

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for PreludePromptCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        _invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let placeholder = "\u{e000}fixture-command-prelude\u{e001}".to_owned();
        Ok(SessionCommandOutput {
            message: "prelude prompt started".to_owned(),
            action: SessionCommandAction::SubmitPrompt {
                content: format!("prelude result: {placeholder}"),
                model_alias: None,
                allowed_tools: Some(vec!["bash".to_owned()]),
                permission_patterns: vec![format!("bash({})", self.command)],
                tool_calls: vec![CommandToolCall {
                    placeholder,
                    name: "bash".to_owned(),
                    arguments: json!({
                        "command": self.command,
                        "cwd": ".",
                        "env": {},
                        "network_domains": [],
                        "sandbox": "sandboxed",
                    }),
                    output_kind: CommandToolOutputKind::ShellInterpolation,
                }],
            },
        })
    }
}

pub(in crate::engine::tests) struct SessionResourceFixture {
    pub(in crate::engine::tests) ended: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for SessionResourceFixture {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        descriptor("session_resource_fixture")
    }

    async fn end_session(&self, session_id: &SessionId) -> Result<(), ToolError> {
        assert_eq!(session_id.0, "fixture-session");
        self.ended.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn session_activity(&self, _session_id: &SessionId) -> Option<String> {
        Some("fixture background resource".to_owned())
    }

    fn observes_session_resources(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::new("unused", Value::Null))
    }
}

pub(in crate::engine::tests) struct PanicQuestionAsker;

#[async_trait]
impl QuestionAsker for PanicQuestionAsker {
    async fn ask(
        &self,
        _request: AskUserInput,
        _cancellation: CancellationToken,
    ) -> Result<String, ToolError> {
        panic!("engine protocol asker must override the tool fallback")
    }
}
