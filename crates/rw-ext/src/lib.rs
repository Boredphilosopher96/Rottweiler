//! Public extension registries and dispatch primitives.
//!
//! Built-ins and third-party extensions use the same command and hook APIs.

mod agent;
mod command;
mod discovery;
mod hook;
mod mode;
mod plugin;
mod plugin_runtime;
mod registry;
mod wasm;
mod wasm_process;
mod workflow;

pub use agent::{
    AgentDefinition, AgentPromptSource, AgentRegistry, AgentRegistryError, LoadedAgent,
    compose_agent_registry,
};

pub use command::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
    CommandRegistryError, CommandSource,
};
pub use discovery::{
    AgentPermissionMode, ArtifactKind, ArtifactLocation, ArtifactOrigin, ArtifactScope,
    CLAUDE_FRONTMATTER_MIGRATION, CommandTemplate, DiscoveredAgent, DiscoveredCommand,
    DiscoveredShellHook, DiscoveredSkill, ExtensionCatalog, ExtensionDiscoveryConfig,
    ExtensionDiscoveryError, InertProjectArtifact, LoadedSkillResource, SkillResource,
    TemplatePart,
};
pub use hook::{
    HookDirective, HookDispatchResult, HookDispatchStatus, HookDispatcher, HookEffect, HookError,
    HookEvent, HookFailure, HookFailurePolicy, HookHandler, HookInvocation, HookRegistration,
    HookRegistrationError,
};
pub use mode::{
    ModeDefinition, ModePermissionOverlay, ModeRegistry, ModeRegistryError, ModeSource,
    compose_mode_registry, parse_mode_toml,
};
pub use plugin::*;
pub use plugin_runtime::*;
pub use registry::*;
pub use wasm::*;
pub use wasm_process::*;
pub use workflow::{
    DiscoveredWorkflow, WorkflowCondition, WorkflowOnFail, WorkflowRunError, WorkflowRunReport,
    WorkflowRunner, WorkflowStep, WorkflowStepArtifact, WorkflowStepExecutionError,
    WorkflowStepExecutor, WorkflowStepReport, WorkflowStepRequest, WorkflowStepTarget,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "extensions";
