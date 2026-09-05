//! Public extension registries and dispatch primitives.
//!
//! Built-ins and third-party extensions use the same command and hook APIs.

mod agent;
mod command;
mod discovery;
mod hook;
mod mode;
mod plugin;
mod plugin_endpoint;
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
    CommandRegistryError,
};
pub use discovery::{
    ArtifactKind, ArtifactLocation, ArtifactOrigin, ArtifactScope, CLAUDE_FRONTMATTER_MIGRATION,
    CommandTemplate, DiscoveredAgent, DiscoveredCommand, DiscoveredShellHook, DiscoveredSkill,
    ExtensionCatalog, ExtensionDiagnostic, ExtensionDiscoveryConfig, ExtensionDiscoveryError,
    InertProjectArtifact, LoadedSkillResource, SkillResource, TemplatePart,
    UninventoriedProjectRoot,
};
pub use hook::{
    HookDirective, HookDispatchResult, HookDispatchStatus, HookDispatcher, HookEffect, HookError,
    HookEvent, HookFailure, HookFailurePolicy, HookHandler, HookInvocation, HookRegistration,
    HookRegistrationError,
};
pub use mode::{
    ModeDefinition, ModeRegistry, ModeRegistryError, ModeSource, compose_mode_registry,
    parse_mode_toml,
};
pub use plugin::*;
pub use plugin_endpoint::{
    PluginConnection, PluginEndpoint, PluginEndpointMetadata, ReadyPluginEndpoint,
};
pub use plugin_runtime::*;
pub use registry::*;
pub use wasm::*;
pub use wasm_process::*;
pub use workflow::{
    DiscoveredWorkflow, WorkflowCondition, WorkflowJournal, WorkflowOnFail, WorkflowRunError,
    WorkflowRunReport, WorkflowRunner, WorkflowStep, WorkflowStepArtifact,
    WorkflowStepExecutionError, WorkflowStepExecutor, WorkflowStepReport, WorkflowStepRequest,
    WorkflowStepTarget,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "extensions";
