use super::allowed_tools::normalize_allowed_tools;
use super::runtime_options::display_agent_error;
use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_core::SessionCommandAction;
use rw_core::SessionCommandContext;
use rw_core::SessionCommandOutput;
use rw_core::builtin_command_registry;
use rw_ext::CommandDescriptor;
use rw_ext::CommandExecutionError;
use rw_ext::CommandHandler;
use rw_ext::CommandInvocation;
use rw_ext::CommandRegistry;
use rw_ext::DiscoveredCommand;
use rw_ext::DiscoveredSkill;
use rw_ext::ExtensionCatalog;
use rw_ext::TemplatePart;
use rw_tools::ToolRegistry;
use rw_types::CommandSource;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) type RuntimeCommandRegistry =
    CommandRegistry<SessionCommandContext, SessionCommandOutput>;

pub(super) const MAX_CUSTOM_COMMAND_PROMPT_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(super) enum CustomPromptDefinition {
    Command(DiscoveredCommand),
    Skill(DiscoveredSkill),
}

impl CustomPromptDefinition {
    pub(super) fn name(&self) -> &str {
        match self {
            Self::Command(command) => command.name(),
            Self::Skill(skill) => skill.name(),
        }
    }

    pub(super) fn origin(&self) -> &rw_ext::ArtifactOrigin {
        match self {
            Self::Command(command) => command.origin(),
            Self::Skill(skill) => skill.origin(),
        }
    }

    pub(super) fn allowed_tools(&self) -> &[String] {
        match self {
            Self::Command(command) => command.allowed_tools(),
            Self::Skill(skill) => skill.allowed_tools(),
        }
    }
}

pub(super) struct CustomPromptCommand {
    pub(super) definition: CustomPromptDefinition,
    pub(super) workspace_roots: Vec<PathBuf>,
    /// `allowed-tools` pre-approvals for the invocation turn.
    pub(super) pre_approvals: Vec<String>,
}

pub(super) struct CustomTemplateRuntime<'a> {
    pub(super) workspace_roots: &'a [PathBuf],
    pub(super) tool_calls: &'a mut Vec<rw_core::CommandToolCall>,
}

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for CustomPromptCommand {
    async fn execute(
        &self,
        session_state: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> std::result::Result<SessionCommandOutput, CommandExecutionError> {
        if session_state.running() {
            return Err(CommandExecutionError::new(
                "turn_running",
                "custom commands require an idle session",
            ));
        }
        let arguments = invocation.arguments();
        let positional = shell_words::split(arguments).map_err(|_| {
            CommandExecutionError::new(
                "invalid_arguments",
                "custom command arguments contain invalid shell-style quoting",
            )
        })?;
        let mut tool_calls = Vec::new();
        let (prompt, model_alias) = match &self.definition {
            CustomPromptDefinition::Command(command) => {
                let template = command.load_template().map_err(extension_command_error)?;
                let mut template_runtime = CustomTemplateRuntime {
                    workspace_roots: &self.workspace_roots,
                    tool_calls: &mut tool_calls,
                };
                let prompt = expand_custom_template(
                    &template,
                    arguments,
                    &positional,
                    &mut template_runtime,
                )?;
                (prompt, command.model().map(str::to_owned))
            }
            CustomPromptDefinition::Skill(skill) => {
                let prompt = skill
                    .render_invocation(arguments)
                    .map_err(extension_command_error)?;
                enforce_custom_prompt_limit(&prompt)?;
                (prompt, None)
            }
        };
        Ok(SessionCommandOutput {
            message: format!("started /{}", self.definition.name()),
            action: SessionCommandAction::SubmitPrompt {
                content: prompt,
                model_alias,
                allowed_tools: None,
                pre_approvals: self.pre_approvals.clone(),
                tool_calls,
            },
        })
    }
}

/// Reports why a declarative command or skill could not load, naming the
/// file and the cause.
pub(super) fn extension_command_error(error: impl std::fmt::Display) -> CommandExecutionError {
    const MAX_MESSAGE_CHARS: usize = 1_024;
    let message = error
        .to_string()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_MESSAGE_CHARS)
        .collect::<String>();
    CommandExecutionError::new("extension_unavailable", message)
}

pub(super) fn expand_custom_template(
    template: &rw_ext::CommandTemplate,
    arguments: &str,
    positional: &[String],
    runtime: &mut CustomTemplateRuntime<'_>,
) -> std::result::Result<String, CommandExecutionError> {
    let mut expanded = String::new();
    for part in template.parts() {
        match part {
            TemplatePart::Text(text) => expanded.push_str(text),
            TemplatePart::Arguments => expanded.push_str(arguments),
            TemplatePart::PositionalArgument(position) => {
                if let Some(argument) = position
                    .checked_sub(1)
                    .and_then(|index| positional.get(index))
                {
                    expanded.push_str(argument);
                }
            }
            TemplatePart::FileInclusion { path } => {
                let display = normalize_custom_command_file_path(runtime.workspace_roots, path)?;
                let placeholder = command_tool_placeholder(
                    runtime.tool_calls.len(),
                    "read",
                    &serde_json::json!({"path": display, "start_line": 1}),
                );
                expanded.push_str(&placeholder);
                runtime.tool_calls.push(rw_core::CommandToolCall {
                    placeholder,
                    name: "read".to_owned(),
                    arguments: serde_json::json!({
                        "path": display.clone(),
                        "start_line": 1,
                    }),
                    output_kind: rw_core::CommandToolOutputKind::FileInclusion { path: display },
                });
            }
            TemplatePart::ShellInterpolation { command } => {
                let arguments = serde_json::json!({
                    "command": command,
                    "cwd": ".",
                    "env": {},
                    "network_domains": [],
                    "sandbox": "sandboxed",
                });
                let placeholder =
                    command_tool_placeholder(runtime.tool_calls.len(), "bash", &arguments);
                expanded.push_str(&placeholder);
                runtime.tool_calls.push(rw_core::CommandToolCall {
                    placeholder,
                    name: "bash".to_owned(),
                    arguments,
                    output_kind: rw_core::CommandToolOutputKind::ShellInterpolation,
                });
            }
        }
        enforce_custom_prompt_limit(&expanded)?;
    }
    Ok(expanded)
}

pub(super) fn command_tool_placeholder(
    index: usize,
    name: &str,
    arguments: &serde_json::Value,
) -> String {
    let mut identity = name.as_bytes().to_vec();
    identity.extend_from_slice(&index.to_le_bytes());
    identity.extend_from_slice(arguments.to_string().as_bytes());
    format!(
        "\u{e000}ROTTWEILER_COMMAND_TOOL_{}_{}\u{e001}",
        index,
        blake3::hash(&identity).to_hex()
    )
}

pub(super) fn enforce_custom_prompt_limit(
    content: &str,
) -> std::result::Result<(), CommandExecutionError> {
    if content.len() > MAX_CUSTOM_COMMAND_PROMPT_BYTES {
        Err(CommandExecutionError::new(
            "command_prompt_too_large",
            "expanded custom command exceeds the prompt size limit",
        ))
    } else {
        Ok(())
    }
}

pub(super) fn normalize_custom_command_file_path(
    roots: &[PathBuf],
    supplied: &str,
) -> std::result::Result<String, CommandExecutionError> {
    let supplied_path = Path::new(supplied);
    if supplied_path.is_absolute()
        || supplied_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(CommandExecutionError::new(
            "file_inclusion_escape",
            "custom command file inclusion must stay inside a workspace root",
        ));
    }
    let mut components = supplied_path.components();
    let (root_index, relative) = if components.next().is_some_and(
        |component| matches!(component, std::path::Component::Normal(name) if name == "@root"),
    ) {
        let std::path::Component::Normal(index) = components.next().ok_or_else(|| {
            CommandExecutionError::new("invalid_file_inclusion", "missing virtual root index")
        })?
        else {
            return Err(CommandExecutionError::new(
                "invalid_file_inclusion",
                "invalid virtual root index",
            ));
        };
        let index = index
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|index| *index > 0)
            .ok_or_else(|| {
                CommandExecutionError::new(
                    "invalid_file_inclusion",
                    "virtual roots use @root/<positive-index>/path",
                )
            })?;
        (index, components.as_path())
    } else {
        (0, supplied_path)
    };
    roots.get(root_index).ok_or_else(|| {
        CommandExecutionError::new("invalid_file_inclusion", "virtual root does not exist")
    })?;
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(CommandExecutionError::new(
            "invalid_file_inclusion",
            "included file path must contain only portable relative components",
        ));
    }
    let display = if root_index == 0 {
        relative.to_string_lossy().into_owned()
    } else {
        format!("@root/{root_index}/{}", relative.display())
    };
    Ok(display)
}

/// Registration order: each project root's locations in precedence order,
/// then the user's.
pub(super) fn extension_origin_rank(origin: &rw_ext::ArtifactOrigin, roots: &[PathBuf]) -> usize {
    const LOCATIONS: usize = 3;
    let location = origin.location().precedence();
    match origin.scope() {
        rw_ext::ArtifactScope::Project => roots
            .iter()
            .position(|root| origin.path().starts_with(root))
            .unwrap_or(roots.len())
            .saturating_mul(LOCATIONS)
            .saturating_add(location),
        rw_ext::ArtifactScope::User => roots
            .len()
            .saturating_mul(LOCATIONS)
            .saturating_add(location),
    }
}

pub(super) fn compose_runtime_commands(
    catalog: &ExtensionCatalog,
    roots: &[PathBuf],
    storage_root: &Path,
    tools: &Arc<ToolRegistry>,
) -> Result<CommandRegistry<SessionCommandContext, SessionCommandOutput>> {
    let mut registry = builtin_command_registry().map_err(display_agent_error)?;
    let primary_workspace = roots
        .first()
        .ok_or_else(|| miette!("project commands require a workspace root"))?;
    crate::project_commands::register_project_commands(
        &mut registry,
        primary_workspace.clone(),
        storage_root.to_path_buf(),
    )
    .map_err(|error| miette!("project commands could not register: {error}"))?;
    crate::workflow_runtime::register_workflow_command(&mut registry, catalog, tools, storage_root)
        .map_err(|error| miette!("workflow command could not register: {error}"))?;
    let mut definitions = catalog
        .commands()
        .cloned()
        .map(CustomPromptDefinition::Command)
        .chain(catalog.skills().cloned().map(CustomPromptDefinition::Skill))
        .collect::<Vec<_>>();
    definitions.sort_by(|left, right| {
        extension_origin_rank(left.origin(), roots)
            .cmp(&extension_origin_rank(right.origin(), roots))
            .then_with(|| {
                matches!(left, CustomPromptDefinition::Skill(_))
                    .cmp(&matches!(right, CustomPromptDefinition::Skill(_)))
            })
            .then_with(|| left.name().cmp(right.name()))
    });
    // A declarative artifact never fails session startup: every refusal is
    // logged here and reported by the extension inventory.
    for definition in definitions {
        if let Some(existing) = registry.resolve(definition.name()) {
            tracing::info!(
                name = definition.name(),
                path = %definition.origin().path().display(),
                existing_source = ?existing.source(),
                "declarative artifact slash name is already registered; skill remains available through the skill tool"
            );
            continue;
        }
        let allowed_tools = normalize_allowed_tools(definition.allowed_tools(), tools);
        for note in &allowed_tools.ignored {
            tracing::info!(
                name = definition.name(),
                path = %definition.origin().path().display(),
                note = note.as_str(),
                "declarative artifact allowed-tools entry ignored"
            );
        }
        let scope = match definition.origin().scope() {
            rw_ext::ArtifactScope::Project => rw_types::ExtensionArtifactScope::Project,
            rw_ext::ArtifactScope::User => rw_types::ExtensionArtifactScope::User,
        };
        let descriptor = match &definition {
            CustomPromptDefinition::Command(command) => command
                .descriptor()
                .with_source(match scope {
                    rw_types::ExtensionArtifactScope::Project => CommandSource::Project,
                    rw_types::ExtensionArtifactScope::User => CommandSource::User,
                })
                .with_scope(scope),
            CustomPromptDefinition::Skill(skill) => {
                CommandDescriptor::new(skill.name(), skill.description())
                    .with_source(CommandSource::Skill)
                    .with_scope(scope)
            }
        };
        let name = definition.name().to_owned();
        let path = definition.origin().path().to_owned();
        if let Err(error) = registry.register(
            descriptor,
            CustomPromptCommand {
                definition,
                workspace_roots: roots.to_vec(),
                pre_approvals: allowed_tools.pre_approvals,
            },
        ) {
            tracing::warn!(
                name = name.as_str(),
                path = %path.display(),
                %error,
                "declarative artifact could not register as a slash command"
            );
        }
    }
    super::extension_inventory::log_extension_inventory(
        &super::extension_inventory::extension_inventory(catalog, Some(tools)),
    );
    Ok(registry)
}
