//! Public extension registries and dispatch primitives.
//!
//! Built-ins and third-party extensions use the same command and hook APIs.

mod command;
mod discovery;
mod hook;

pub use command::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
    CommandRegistryError,
};
pub use discovery::{
    ArtifactKind, ArtifactLocation, ArtifactOrigin, ArtifactScope, CLAUDE_FRONTMATTER_MIGRATION,
    CommandTemplate, DiscoveredCommand, DiscoveredShellHook, DiscoveredSkill, ExtensionCatalog,
    ExtensionDiscoveryConfig, ExtensionDiscoveryError, InertProjectArtifact, LoadedSkillResource,
    SkillResource, TemplatePart,
};
pub use hook::{
    HookDirective, HookDispatchResult, HookDispatchStatus, HookDispatcher, HookEffect, HookError,
    HookEvent, HookFailure, HookFailurePolicy, HookHandler, HookInvocation, HookRegistration,
    HookRegistrationError,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "extensions";
