use crate::PermissionGate;
use crate::engine::commands::FolderTrustController;
use crate::engine::commands::SessionCommandContext;
use crate::engine::commands::SessionCommandOutput;
use crate::engine::commands::WorkspaceRootController;
use crate::engine::commands::WorkspaceRuntimeGeneration;
use crate::engine::durability::SessionEventSink;
use crate::engine::event_clock::EventClock;
use crate::engine::model::ModelDriver;
use crate::engine::mutation_checkpoints::MutationCheckpointCoordinator;
use crate::engine::projection::SessionRecoveredState;
use crate::engine::redaction::SecretRedactor;
use crate::engine::session_extension::SessionExtensionController;
use crate::engine::session_extension::SessionExtensionSnapshot;
use crate::engine::session_resources::SessionResources;
use rw_ext::CommandRegistry;
use rw_ext::HookDispatcher;
use rw_ext::ModeRegistry;
use rw_ext::ModeSource;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::ModeId;
use rw_types::Role;
use rw_types::SessionId;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::config::ThinkingLevel;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// Dependencies and guardrails for one headless session actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupNotification {
    pub plugin_id: String,
    pub status: String,
    pub title: String,
    pub message: String,
}

pub struct SessionActorConfig {
    pub ui: Arc<dyn crate::ui::UiRegistry>,
    pub ui_tool_source: Arc<dyn crate::ui::UiToolSource>,
    pub session_id: SessionId,
    /// Immutable root session whose cap covers this session and its descendants.
    pub budget_session_id: SessionId,
    pub workspace_root: PathBuf,
    pub additional_workspace_roots: Vec<PathBuf>,
    pub workspace_generation: u64,
    pub initial_session_context: Vec<Turn>,
    pub startup_notifications: Vec<StartupNotification>,
    pub model_alias: String,
    pub model: Arc<dyn ModelDriver>,
    pub tools: Arc<ToolRegistry>,
    pub permissions: Arc<PermissionGate>,
    pub hooks: Arc<HookDispatcher>,
    pub commands: Arc<CommandRegistry<SessionCommandContext, SessionCommandOutput>>,
    pub modes: Arc<ModeRegistry>,
    pub event_sink: Arc<dyn SessionEventSink>,
    pub history: Arc<dyn crate::recovery::SessionHistory>,
    pub event_clock: Arc<dyn EventClock>,
    pub provider_admission: Arc<dyn crate::provider_admission::ProviderAdmission>,
    pub secret_redactor: Arc<dyn SecretRedactor>,
    pub checkpoints: Arc<dyn MutationCheckpointCoordinator>,
    pub folder_trust: Arc<dyn FolderTrustController>,
    pub workspace_roots: Arc<dyn WorkspaceRootController>,
    pub extension_development: Arc<dyn SessionExtensionController>,
    pub resources: Arc<dyn SessionResources>,
    pub recovered: SessionRecoveredState,
    pub max_turns: usize,
    pub identical_tool_failure_limit: usize,
    pub max_output_tokens: u32,
    pub thinking: ThinkingLevel,
    pub event_capacity: usize,
}

impl fmt::Debug for SessionActorConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionActorConfig")
            .field("session_id", &self.session_id)
            .field("workspace_root", &self.workspace_root)
            .field(
                "additional_workspace_roots",
                &self.additional_workspace_roots,
            )
            .field("workspace_generation", &self.workspace_generation)
            .field("initial_session_context", &self.initial_session_context)
            .field("startup_notifications", &self.startup_notifications)
            .field("model_alias", &self.model_alias)
            .field("recovered", &self.recovered)
            .field("max_turns", &self.max_turns)
            .field(
                "identical_tool_failure_limit",
                &self.identical_tool_failure_limit,
            )
            .field("max_output_tokens", &self.max_output_tokens)
            .field("thinking", &self.thinking)
            .field("event_capacity", &self.event_capacity)
            .finish_non_exhaustive()
    }
}

impl SessionActorConfig {
    pub(in crate::engine) fn with_model_alias(&self, model_alias: String) -> Self {
        Self {
            ui: Arc::clone(&self.ui),
            ui_tool_source: Arc::clone(&self.ui_tool_source),
            session_id: self.session_id.clone(),
            budget_session_id: self.budget_session_id.clone(),
            workspace_root: self.workspace_root.clone(),
            additional_workspace_roots: self.additional_workspace_roots.clone(),
            workspace_generation: self.workspace_generation,
            initial_session_context: self.initial_session_context.clone(),
            startup_notifications: self.startup_notifications.clone(),
            model_alias,
            model: Arc::clone(&self.model),
            tools: Arc::clone(&self.tools),
            permissions: Arc::clone(&self.permissions),
            hooks: Arc::clone(&self.hooks),
            commands: Arc::clone(&self.commands),
            modes: Arc::clone(&self.modes),
            event_sink: Arc::clone(&self.event_sink),
            history: Arc::clone(&self.history),
            event_clock: Arc::clone(&self.event_clock),
            provider_admission: Arc::clone(&self.provider_admission),
            secret_redactor: Arc::clone(&self.secret_redactor),
            checkpoints: Arc::clone(&self.checkpoints),
            folder_trust: Arc::clone(&self.folder_trust),
            workspace_roots: Arc::clone(&self.workspace_roots),
            extension_development: Arc::clone(&self.extension_development),
            resources: Arc::clone(&self.resources),
            recovered: self.recovered.clone(),
            max_turns: self.max_turns,
            identical_tool_failure_limit: self.identical_tool_failure_limit,
            max_output_tokens: self.max_output_tokens,
            thinking: self.thinking,
            event_capacity: self.event_capacity,
        }
    }

    pub(in crate::engine) fn with_workspace_generation(
        &self,
        generation: &WorkspaceRuntimeGeneration,
        active_mode: &ModeId,
    ) -> Self {
        let mut configured = self.with_model_alias(self.model_alias.clone());
        configured.workspace_root.clone_from(&generation.roots[0]);
        configured.additional_workspace_roots = generation.roots.iter().skip(1).cloned().collect();
        configured.workspace_generation = generation.generation;
        configured.model = Arc::clone(&generation.model);
        configured.ui = Arc::clone(&generation.ui);
        configured.tools = Arc::clone(&generation.tools);
        configured.hooks = Arc::clone(&generation.hooks);
        configured.commands = Arc::clone(&generation.commands);
        configured.modes = self.modes.get(&active_mode.0).map_or_else(
            || Arc::clone(&generation.modes),
            |definition| Arc::new(generation.modes.with_pinned(definition.clone())),
        );
        configured.permissions = Arc::clone(&generation.permissions);
        configured.checkpoints = Arc::clone(&generation.checkpoints);
        configured.folder_trust = Arc::clone(&generation.folder_trust);
        configured
            .initial_session_context
            .extend(generation.supplemental_context.iter().cloned());
        configured
    }

    pub(in crate::engine) fn with_extension_snapshot(
        &self,
        snapshot: &SessionExtensionSnapshot,
    ) -> Self {
        let mut configured = self.with_model_alias(self.model_alias.clone());
        configured.model = Arc::clone(&snapshot.model);
        configured.tools = Arc::clone(&snapshot.tools);
        configured.hooks = Arc::clone(&snapshot.hooks);
        configured.commands = Arc::clone(&snapshot.commands);
        configured.ui = Arc::clone(&snapshot.ui);
        configured
    }

    pub(super) fn with_model_alias_and_mode(&self, model_alias: String, mode_id: &ModeId) -> Self {
        let mut configured = self.with_model_alias(model_alias);
        let Some(mode) = configured.modes.get(&mode_id.0) else {
            return configured;
        };
        // Execute is the base policy already present in the canonical system
        // prompt. Preserve that stable cache prefix for the embedded default;
        // an extension overriding `execute` still contributes its fragment.
        if mode.id().0 == "execute" && matches!(mode.source(), ModeSource::Embedded { .. }) {
            return configured;
        }
        if let Some(system) = configured
            .initial_session_context
            .iter_mut()
            .find(|turn| turn.role == Role::System)
        {
            system.blocks.push(Block::Text {
                text: mode.prompt().to_owned(),
            });
        } else {
            configured.initial_session_context.insert(
                0,
                Turn {
                    role: Role::System,
                    blocks: vec![Block::Text {
                        text: mode.prompt().to_owned(),
                    }],
                    meta: TurnMeta::default(),
                },
            );
        }
        configured
    }

    pub(in crate::engine) fn with_model_route_and_mode(
        &self,
        model_alias: String,
        provider: Option<String>,
        mode_id: &ModeId,
    ) -> Self {
        let mut configured = self.with_model_alias_and_mode(model_alias, mode_id);
        configured.recovered.provider = provider;
        configured
    }
}
