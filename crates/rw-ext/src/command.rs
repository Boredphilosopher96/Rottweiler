use std::{collections::BTreeMap, panic::AssertUnwindSafe, sync::Arc};

use async_trait::async_trait;
use futures_util::FutureExt;
use rw_types::CommandSource;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandDescriptor {
    name: String,
    description: String,
    argument_hint: Option<String>,
    source: CommandSource,
    host_tools: Arc<[String]>,
}

impl CommandDescriptor {
    /// Creates a descriptor. Names are validated when registered.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            argument_hint: None,
            source: CommandSource::Builtin,
            host_tools: Arc::from([]),
        }
    }

    /// Exact host tools this command may request through its invocation capability.
    #[must_use]
    pub fn with_host_tools(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.host_tools = names.into_iter().collect();
        self
    }

    /// Adds the concise argument hint shown alongside the command.
    #[must_use]
    pub fn with_argument_hint(mut self, argument_hint: impl Into<String>) -> Self {
        self.argument_hint = Some(argument_hint.into());
        self
    }

    #[must_use]
    pub const fn with_source(mut self, source: CommandSource) -> Self {
        self.source = source;
        self
    }

    /// Canonical command name, without the leading slash.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Human-readable command description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Optional concise argument syntax.
    #[must_use]
    pub fn argument_hint(&self) -> Option<&str> {
        self.argument_hint.as_deref()
    }

    #[must_use]
    pub const fn source(&self) -> CommandSource {
        self.source
    }
}

/// A parsed command invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandInvocation {
    origin: Option<rw_types::extension_invocation::ExtensionInvocationId>,
    name: String,
    arguments: String,
}

impl CommandInvocation {
    #[must_use]
    pub fn origin(&self) -> Option<&rw_types::extension_invocation::ExtensionInvocationId> {
        self.origin.as_ref()
    }

    /// Canonical command name, without the leading slash.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Arguments with leading separator whitespace removed.
    #[must_use]
    pub fn arguments(&self) -> &str {
        &self.arguments
    }
}

/// A command implementation usable by built-ins and extensions alike.
#[async_trait]
pub trait CommandHandler<Context, Output>: Send + Sync {
    /// Executes one parsed invocation against the engine-owned context.
    async fn execute(
        &self,
        context: &mut Context,
        invocation: CommandInvocation,
    ) -> Result<Output, CommandExecutionError>;
}

/// An implementation-reported command failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{code}: {message}")]
pub struct CommandExecutionError {
    code: String,
    message: String,
}

impl CommandExecutionError {
    /// Creates a stable, client-safe command error.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Stable machine-readable error code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Client-safe explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Command registration or invocation failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CommandRegistryError {
    /// A dynamic command source could not admit its metadata construction.
    #[error("command metadata admission exceeded")]
    Admission,
    /// A command name was empty or contained unsupported characters.
    #[error("invalid command name `{name}`; use lowercase ASCII letters, digits, '-', '_' or '.'")]
    InvalidName { name: String },
    /// The canonical name was already registered.
    #[error("command `{name}` is already registered")]
    Duplicate { name: String },
    /// The input was not a slash command.
    #[error("input is not a slash command")]
    NotACommand,
    /// No exact registration matched the parsed name.
    #[error("unknown command `{name}`")]
    Unknown { name: String },
    /// The selected handler rejected or failed the command.
    #[error("command `{name}` failed: {source}")]
    Execution {
        name: String,
        #[source]
        source: CommandExecutionError,
    },
}

struct RegisteredCommand<Context, Output> {
    descriptor: CommandDescriptor,
    handler: Arc<dyn CommandHandler<Context, Output>>,
}

/// Descriptor-only view of one immutable registry generation.
///
/// The shared registry retains each descriptor and its handler's source owner;
/// this view exposes no parsing or execution capability and copies no metadata.
pub struct CommandCatalog<Context, Output> {
    registry: Arc<CommandRegistry<Context, Output>>,
}
impl<Context, Output> Clone for CommandCatalog<Context, Output> {
    fn clone(&self) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
        }
    }
}
impl<Context, Output> CommandCatalog<Context, Output> {
    /// Iterates borrowed descriptors in canonical order.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &CommandDescriptor> {
        self.registry.descriptors()
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.registry.commands.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registry.commands.is_empty()
    }
}

/// Deterministic slash-command registry shared by built-ins and extensions.
///
/// Resolution is an exact, case-sensitive lookup. Introspection is sorted by
/// canonical name and duplicate names are rejected instead of load-order
/// overriding an existing command.
#[derive(Clone)]
pub struct CommandRegistry<Context, Output> {
    commands: BTreeMap<Arc<str>, Arc<RegisteredCommand<Context, Output>>>,
}

impl<Context, Output> Default for CommandRegistry<Context, Output> {
    fn default() -> Self {
        Self {
            commands: BTreeMap::new(),
        }
    }
}

impl<Context, Output> CommandRegistry<Context, Output> {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Captures this immutable generation without copying its metadata.
    #[must_use]
    pub fn catalog(self: &Arc<Self>) -> CommandCatalog<Context, Output> {
        CommandCatalog {
            registry: Arc::clone(self),
        }
    }

    /// Registers a command through the common built-in/extension API.
    ///
    /// # Errors
    ///
    /// Returns [`CommandRegistryError::InvalidName`] for a non-canonical name,
    /// or [`CommandRegistryError::Duplicate`] if the exact name already exists.
    pub fn register<Handler>(
        &mut self,
        descriptor: CommandDescriptor,
        handler: Handler,
    ) -> Result<(), CommandRegistryError>
    where
        Handler: CommandHandler<Context, Output> + 'static,
    {
        self.register_shared(descriptor, Arc::new(handler))
    }

    /// Registers a shared handler, as used by forwarding bridges.
    ///
    /// # Errors
    ///
    /// Returns [`CommandRegistryError::InvalidName`] for a non-canonical name,
    /// or [`CommandRegistryError::Duplicate`] if the exact name already exists.
    pub fn register_shared(
        &mut self,
        descriptor: CommandDescriptor,
        handler: Arc<dyn CommandHandler<Context, Output>>,
    ) -> Result<(), CommandRegistryError> {
        // Descriptor bytes retire before the handler that owns their admission.
        // Registry clones share this entire registration and its key backing.
        let registration = Arc::new(RegisteredCommand {
            descriptor,
            handler,
        });
        validate_name(registration.descriptor.name())?;
        let name = registration.descriptor.name();
        if self.commands.contains_key(name) {
            return Err(CommandRegistryError::Duplicate {
                name: name.to_owned(),
            });
        }
        self.commands.insert(Arc::from(name), registration);
        Ok(())
    }

    /// Removes and reports whether an exact registration existed.
    pub fn unregister(&mut self, name: &str) -> bool {
        canonical_lookup_name(name).is_some_and(|name| self.commands.remove(name).is_some())
    }

    /// Returns an exact command descriptor, accepting an optional leading `/`.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&CommandDescriptor> {
        canonical_lookup_name(name)
            .and_then(|name| self.commands.get(name))
            .map(|registered| &registered.descriptor)
    }

    /// Returns descriptors in canonical-name order, independent of load order.
    #[must_use]
    pub fn descriptors(&self) -> impl ExactSizeIterator<Item = &CommandDescriptor> {
        self.commands
            .values()
            .map(|registered| &registered.descriptor)
    }

    /// Parses and dispatches one slash-command line.
    ///
    /// # Errors
    ///
    /// Returns a [`CommandRegistryError`] when the line is not a valid slash
    /// command, has no exact registration, or its handler reports a failure.
    pub async fn dispatch_line(
        &self,
        context: &mut Context,
        line: &str,
    ) -> Result<Output, CommandRegistryError> {
        self.bind_line(line)?.execute(context).await
    }

    /// Binds a parsed slash command before asynchronous execution.
    /// # Errors
    /// Rejects invalid syntax or an absent exact registration.
    pub fn bind_line(
        &self,
        line: &str,
    ) -> Result<BoundCommand<Context, Output>, CommandRegistryError> {
        let invocation = parse_invocation(line)?;
        self.bind(&invocation.name, invocation.arguments)
    }

    /// Captures the exact registered handler and inert arguments at admission.
    /// Subsequent registry replacement cannot retarget the invocation.
    ///
    /// # Errors
    /// Rejects noncanonical or absent command names.
    pub fn bind(
        &self,
        name: &str,
        arguments: String,
    ) -> Result<BoundCommand<Context, Output>, CommandRegistryError> {
        validate_name(name)?;
        let registered = self
            .commands
            .get(name)
            .ok_or_else(|| CommandRegistryError::Unknown {
                name: name.to_owned(),
            })?;
        Ok(BoundCommand {
            host_tools: Arc::clone(&registered.descriptor.host_tools),
            handler: Arc::clone(&registered.handler),
            invocation: CommandInvocation {
                origin: None,
                name: name.to_owned(),
                arguments,
            },
        })
    }
}

/// One admitted command bound to its actual implementation, never a later name lookup.
pub struct BoundCommand<Context, Output> {
    host_tools: Arc<[String]>,
    handler: Arc<dyn CommandHandler<Context, Output>>,
    invocation: CommandInvocation,
}
impl<Context, Output> BoundCommand<Context, Output> {
    #[must_use]
    pub fn host_tools(&self) -> Arc<[String]> {
        Arc::clone(&self.host_tools)
    }
    #[must_use]
    pub fn with_origin(
        mut self,
        origin: rw_types::extension_invocation::ExtensionInvocationId,
    ) -> Self {
        self.invocation.origin = Some(origin);
        self
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.invocation.name()
    }

    /// Executes the captured implementation through the shared panic boundary.
    ///
    /// # Errors
    /// Reports implementation rejection or panic.
    pub async fn execute(&self, context: &mut Context) -> Result<Output, CommandRegistryError> {
        let name = self.invocation.name.clone();
        match AssertUnwindSafe(self.handler.execute(context, self.invocation.clone()))
            .catch_unwind()
            .await
        {
            Ok(result) => result.map_err(|source| CommandRegistryError::Execution { name, source }),
            Err(_) => Err(CommandRegistryError::Execution {
                name,
                source: CommandExecutionError::new("panic", "command implementation panicked"),
            }),
        }
    }
}

fn validate_name(name: &str) -> Result<(), CommandRegistryError> {
    let valid = !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        });
    if valid {
        Ok(())
    } else {
        Err(CommandRegistryError::InvalidName {
            name: name.to_owned(),
        })
    }
}

fn canonical_lookup_name(name: &str) -> Option<&str> {
    let name = name.strip_prefix('/').unwrap_or(name);
    (!name.is_empty() && !name.bytes().any(|byte| byte.is_ascii_whitespace())).then_some(name)
}

fn parse_invocation(line: &str) -> Result<CommandInvocation, CommandRegistryError> {
    let trimmed = line.trim_start();
    let Some(without_slash) = trimmed.strip_prefix('/') else {
        return Err(CommandRegistryError::NotACommand);
    };
    let split_at = without_slash
        .find(char::is_whitespace)
        .unwrap_or(without_slash.len());
    let name = &without_slash[..split_at];
    validate_name(name)?;
    Ok(CommandInvocation {
        origin: None,
        name: name.to_owned(),
        arguments: without_slash[split_at..].trim_start().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use async_trait::async_trait;

    use super::*;

    #[test]
    fn registry_clone_shares_descriptor_and_registration_custody() {
        let mut registry = CommandRegistry::new();
        registry
            .register(
                CommandDescriptor::new("large", "x".repeat(128 * 1024)),
                Append("ok"),
            )
            .expect("register");
        let cloned = registry.clone();
        assert!(std::ptr::eq(
            registry.resolve("large").expect("original"),
            cloned.resolve("large").expect("clone")
        ));
        let owner = Arc::downgrade(registry.commands.get("large").expect("owner"));
        assert!(registry.unregister("large"));
        assert!(owner.upgrade().is_some());
        assert_eq!(
            cloned
                .resolve("large")
                .expect("retained")
                .description()
                .len(),
            128 * 1024
        );
        drop(cloned);
        assert!(owner.upgrade().is_none());
    }

    #[test]
    fn descriptor_snapshot_retains_exact_registration_after_generation_replacement() {
        let mut registry = CommandRegistry::new();
        registry
            .register(
                CommandDescriptor::new("large", "x".repeat(128 * 1024)),
                Append("old"),
            )
            .expect("register");
        let registry = Arc::new(registry);
        let source = Arc::downgrade(registry.commands.get("large").expect("source"));
        let snapshot = registry.catalog();
        let clone = snapshot.clone();
        assert!(std::ptr::eq(
            snapshot.iter().next().expect("descriptor"),
            registry.resolve("large").expect("registered")
        ));
        let mut replacement = (*registry).clone();
        assert!(replacement.unregister("large"));
        assert!(Arc::new(replacement).catalog().is_empty());
        drop(registry);
        drop(snapshot);
        assert!(
            source.upgrade().is_some(),
            "old catalog retains its metadata source"
        );
        assert_eq!(clone.len(), 1);
        assert_eq!(
            clone
                .iter()
                .next()
                .expect("old descriptor")
                .description()
                .len(),
            128 * 1024
        );
        drop(clone);
        assert!(
            source.upgrade().is_none(),
            "final catalog retires its source"
        );
    }

    struct Append(&'static str);

    #[async_trait]
    impl CommandHandler<Vec<String>, String> for Append {
        async fn execute(
            &self,
            context: &mut Vec<String>,
            invocation: CommandInvocation,
        ) -> Result<String, CommandExecutionError> {
            context.push(format!("{}:{}", self.0, invocation.arguments()));
            Ok(invocation.name().to_owned())
        }
    }

    struct Reject;

    #[async_trait]
    impl CommandHandler<Vec<String>, String> for Reject {
        async fn execute(
            &self,
            _context: &mut Vec<String>,
            _invocation: CommandInvocation,
        ) -> Result<String, CommandExecutionError> {
            Err(CommandExecutionError::new("denied", "not now"))
        }
    }

    #[tokio::test]
    async fn built_ins_and_extensions_use_the_same_registration_and_dispatch_path() {
        let mut registry = CommandRegistry::new();
        registry
            .register(
                CommandDescriptor::new("rewind", "built in"),
                Append("built-in"),
            )
            .expect("valid built-in registration");
        registry
            .register(
                CommandDescriptor::new("review", "extension"),
                Append("extension"),
            )
            .expect("valid extension registration");
        let mut context = Vec::new();

        assert_eq!(
            registry.dispatch_line(&mut context, "  /rewind  2 ").await,
            Ok("rewind".to_owned())
        );
        assert_eq!(
            registry.dispatch_line(&mut context, "/review src/").await,
            Ok("review".to_owned())
        );
        assert_eq!(context, ["built-in:2 ", "extension:src/"]);
    }

    #[tokio::test]
    async fn captured_command_cannot_retarget_a_replaced_registration() {
        let mut registry = CommandRegistry::new();
        registry
            .register(CommandDescriptor::new("open", "Open"), Append("first"))
            .expect("first registration");
        let admitted = registry
            .bind("open", "{\"path\":\"a b\"}".into())
            .expect("bound action");
        assert!(registry.unregister("open"));
        registry
            .register(CommandDescriptor::new("open", "Open"), Append("second"))
            .expect("replacement registration");
        let mut context = Vec::new();
        admitted
            .execute(&mut context)
            .await
            .expect("admitted implementation");
        assert_eq!(context, ["first:{\"path\":\"a b\"}"]);
        registry
            .dispatch_line(&mut context, "/open later")
            .await
            .expect("new implementation");
        assert_eq!(context[1], "second:later");
    }

    #[test]
    fn descriptors_and_resolution_are_deterministic_and_exact() {
        let mut registry = CommandRegistry::<(), ()>::new();
        registry
            .register(CommandDescriptor::new("zeta", "z"), Noop)
            .expect("valid registration");
        registry
            .register(
                CommandDescriptor::new("alpha", "a").with_argument_hint("[file]"),
                Noop,
            )
            .expect("valid registration");

        let names: Vec<_> = registry
            .descriptors()
            .map(CommandDescriptor::name)
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert_eq!(
            registry.resolve("/alpha").map(CommandDescriptor::name),
            Some("alpha")
        );
        assert!(registry.resolve("alp").is_none());
        assert!(registry.resolve("/Alpha").is_none());
        assert_eq!(
            registry
                .resolve("alpha")
                .and_then(CommandDescriptor::argument_hint),
            Some("[file]")
        );
    }

    struct Noop;

    #[async_trait]
    impl CommandHandler<(), ()> for Noop {
        async fn execute(
            &self,
            _context: &mut (),
            _invocation: CommandInvocation,
        ) -> Result<(), CommandExecutionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn dispatch_distinguishes_non_commands_unknown_commands_and_handler_errors() {
        let mut registry = CommandRegistry::new();
        registry
            .register(CommandDescriptor::new("reject", "rejects"), Reject)
            .expect("valid registration");
        let mut context = Vec::new();

        assert_eq!(
            registry.dispatch_line(&mut context, "hello").await,
            Err(CommandRegistryError::NotACommand)
        );
        assert_eq!(
            registry.dispatch_line(&mut context, "/missing").await,
            Err(CommandRegistryError::Unknown {
                name: "missing".to_owned()
            })
        );
        assert_eq!(
            registry.dispatch_line(&mut context, "/reject").await,
            Err(CommandRegistryError::Execution {
                name: "reject".to_owned(),
                source: CommandExecutionError::new("denied", "not now")
            })
        );
    }

    #[test]
    fn invalid_and_duplicate_names_are_rejected_without_overriding() {
        let mut registry = CommandRegistry::<(), ()>::new();
        let invalid = registry.register(CommandDescriptor::new("Bad Name", "bad"), Noop);
        assert!(matches!(
            invalid,
            Err(CommandRegistryError::InvalidName { .. })
        ));

        registry
            .register(CommandDescriptor::new("same", "first"), Noop)
            .expect("valid registration");
        assert_eq!(
            registry.register(CommandDescriptor::new("same", "second"), Noop),
            Err(CommandRegistryError::Duplicate {
                name: "same".to_owned()
            })
        );
        assert_eq!(
            registry.resolve("same").map(CommandDescriptor::description),
            Some("first")
        );
        assert!(registry.unregister("/same"));
        assert!(!registry.unregister("same"));
    }
}
