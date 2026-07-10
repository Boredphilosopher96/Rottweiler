//! Public extension registries and dispatch primitives.
//!
//! Built-ins and third-party extensions use the same command and hook APIs.

mod command;
mod hook;

pub use command::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
    CommandRegistryError,
};
pub use hook::{
    HookDirective, HookDispatchResult, HookDispatchStatus, HookDispatcher, HookError, HookEvent,
    HookFailure, HookFailurePolicy, HookHandler, HookInvocation, HookRegistration,
    HookRegistrationError,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "extensions";
