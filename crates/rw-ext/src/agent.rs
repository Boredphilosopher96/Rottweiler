use std::collections::{BTreeMap, BTreeSet};

use rw_tools::{ToolRegistry, validate_mcp_virtual_tool};
use thiserror::Error;

use crate::{AgentPermissionMode, DiscoveredAgent, ExtensionCatalog, ExtensionDiscoveryError};

const EXPLORE_PROMPT: &str = "Explore the requested area carefully. Prefer read-only tools, cite concrete paths and evidence, and return a concise finding summary.";
const PLAN_PROMPT: &str = "Produce an implementation plan grounded in repository evidence. Do not mutate files. Identify dependencies, risks, and verification steps.";
const GENERAL_PROMPT: &str = "Complete the delegated task within its stated scope. Preserve unrelated work, use the least privilege needed, verify the result, and report touched files.";

/// The prompt source behind an agent registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentPromptSource {
    Embedded(&'static str),
    Declarative(Box<DiscoveredAgent>),
}

/// One agent definition registered through the public extension API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDefinition {
    name: String,
    description: String,
    /// An explicit declarative model alias. Built-in agents leave this unset
    /// so each launch inherits the parent turn's currently selected live
    /// model instead of depending on a synthetic alias.
    model: Option<String>,
    tools: Vec<String>,
    permission_mode: AgentPermissionMode,
    max_turns: usize,
    prompt: AgentPromptSource,
}

impl AgentDefinition {
    #[must_use]
    pub fn embedded(
        name: impl Into<String>,
        description: impl Into<String>,
        tools: Vec<String>,
        permission_mode: AgentPermissionMode,
        max_turns: usize,
        system_prompt: &'static str,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            model: None,
            tools,
            permission_mode,
            max_turns,
            prompt: AgentPromptSource::Embedded(system_prompt),
        }
    }

    #[must_use]
    pub fn declarative(agent: DiscoveredAgent) -> Self {
        Self {
            name: agent.name().to_owned(),
            description: agent.description().to_owned(),
            model: Some(agent.model().to_owned()),
            tools: agent.tools().to_vec(),
            permission_mode: agent.permission_mode(),
            max_turns: agent.max_turns(),
            prompt: AgentPromptSource::Declarative(Box::new(agent)),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    #[must_use]
    pub const fn permission_mode(&self) -> AgentPermissionMode {
        self.permission_mode
    }

    #[must_use]
    pub const fn max_turns(&self) -> usize {
        self.max_turns
    }

    /// Resolves the system prompt at selection time.
    ///
    /// # Errors
    ///
    /// Declarative prompts fail closed if their source changed after discovery.
    pub fn load(&self) -> Result<LoadedAgent, AgentRegistryError> {
        let system_prompt = match &self.prompt {
            AgentPromptSource::Embedded(prompt) => (*prompt).to_owned(),
            AgentPromptSource::Declarative(agent) => agent.load_system_prompt()?,
        };
        Ok(LoadedAgent {
            name: self.name.clone(),
            description: self.description.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            permission_mode: self.permission_mode,
            max_turns: self.max_turns,
            system_prompt,
        })
    }
}

/// Fully loaded agent request passed to the orchestrator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedAgent {
    pub name: String,
    pub description: String,
    /// `None` means inherit the exact model selected for the parent turn.
    /// Declarative agents retain their explicit alias in `Some`.
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub permission_mode: AgentPermissionMode,
    pub max_turns: usize,
    pub system_prompt: String,
}

/// Common registry used for built-in and declarative agents.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentRegistry {
    definitions: BTreeMap<String, AgentDefinition>,
}

impl AgentRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one definition.
    ///
    /// # Errors
    ///
    /// Rejects invalid names, limits, aliases, and duplicate definitions.
    pub fn register(&mut self, definition: AgentDefinition) -> Result<(), AgentRegistryError> {
        validate_definition(&definition)?;
        let name = definition.name.clone();
        if self.definitions.insert(name.clone(), definition).is_some() {
            return Err(AgentRegistryError::Duplicate { name });
        }
        Ok(())
    }

    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&AgentDefinition> {
        self.definitions.get(name)
    }

    /// Loads an exact definition for spawning.
    ///
    /// # Errors
    ///
    /// Returns unknown-name or lazy-source failures unchanged.
    pub fn load(&self, name: &str) -> Result<LoadedAgent, AgentRegistryError> {
        self.resolve(name)
            .ok_or_else(|| AgentRegistryError::Unknown {
                name: name.to_owned(),
            })?
            .load()
    }

    #[must_use]
    pub fn definitions(&self) -> impl ExactSizeIterator<Item = &AgentDefinition> {
        self.definitions.values()
    }

    /// Validates every explicit model role before a child session is created.
    ///
    /// # Errors
    ///
    /// Returns the first definition whose model alias is not configured.
    pub fn validate_model_aliases<'a>(
        &self,
        aliases: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), AgentRegistryError> {
        let aliases = aliases.into_iter().collect::<BTreeSet<_>>();
        for definition in self.definitions.values() {
            let Some(model) = definition.model() else {
                continue;
            };
            if !aliases.contains(model) {
                return Err(AgentRegistryError::UnknownModelAlias {
                    agent: definition.name.clone(),
                    model: model.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Resolves tool allowlists against the exact production registry.
    /// Optional built-in tools degrade gracefully; a declarative typo fails.
    ///
    /// # Errors
    ///
    /// Returns the first unavailable declarative tool name.
    pub fn resolve_tools(&mut self, tools: &ToolRegistry) -> Result<(), AgentRegistryError> {
        self.resolve_tool_names(
            tools
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name),
        )
    }

    /// Resolves allowlists against a precomputed production tool-name set.
    /// This supports late-bound orchestration tools without weakening typo
    /// detection for declarative agents.
    ///
    /// # Errors
    ///
    /// Returns the first unavailable declarative tool name.
    pub fn resolve_tool_names(
        &mut self,
        names: impl IntoIterator<Item = String>,
    ) -> Result<(), AgentRegistryError> {
        let names = names.into_iter().collect::<BTreeSet<_>>();
        for definition in self.definitions.values_mut() {
            let available = |name: &String| {
                !matches!(name.as_str(), "tool_search" | "mcp_call")
                    && (names.contains(name)
                        || (name.starts_with("mcp:") && validate_mcp_virtual_tool(name).is_ok()))
            };
            match &definition.prompt {
                AgentPromptSource::Embedded(_) => {
                    definition.tools.retain(&available);
                }
                AgentPromptSource::Declarative(_) => {
                    if let Some(name) = definition.tools.iter().find(|name| !available(name)) {
                        return Err(AgentRegistryError::UnknownTool {
                            agent: definition.name.clone(),
                            tool: name.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// Builds the runtime registry. Declarative definitions use ADR-014
/// precedence and may intentionally shadow a built-in with the same name.
///
/// # Errors
///
/// Returns an invalid or duplicate-definition error if the catalog cannot be
/// represented by the common registry.
pub fn compose_agent_registry(
    catalog: &ExtensionCatalog,
) -> Result<AgentRegistry, AgentRegistryError> {
    let mut registry = AgentRegistry::new();
    for agent in catalog.agents().cloned() {
        registry.register(AgentDefinition::declarative(agent))?;
    }
    for builtin in builtin_agents() {
        if registry.resolve(builtin.name()).is_none() {
            registry.register(builtin)?;
        }
    }
    Ok(registry)
}

fn builtin_agents() -> [AgentDefinition; 3] {
    [
        AgentDefinition::embedded(
            "explore",
            "Read-only repository exploration",
            vec![
                "read".to_owned(),
                "grep".to_owned(),
                "glob".to_owned(),
                "ls".to_owned(),
            ],
            AgentPermissionMode::Discuss,
            16,
            EXPLORE_PROMPT,
        ),
        AgentDefinition::embedded(
            "plan",
            "Evidence-based implementation planning",
            vec![
                "read".to_owned(),
                "grep".to_owned(),
                "glob".to_owned(),
                "ls".to_owned(),
            ],
            AgentPermissionMode::Plan,
            24,
            PLAN_PROMPT,
        ),
        AgentDefinition::embedded(
            "general",
            "General delegated coding work",
            [
                "read",
                "write",
                "edit",
                "multi_edit",
                "grep",
                "glob",
                "ls",
                "bash",
                "webfetch",
                "websearch",
                "todo",
                "spawn_agent",
                "apply_worktree_diff",
                "symbols",
                "diagnostics",
                "definition",
                "references",
                "rename",
                "submit_plan",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            AgentPermissionMode::Execute,
            32,
            GENERAL_PROMPT,
        ),
    ]
}

fn validate_definition(definition: &AgentDefinition) -> Result<(), AgentRegistryError> {
    let name_valid = !definition.name.is_empty()
        && definition.name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        });
    if !name_valid
        || definition.description.trim().is_empty()
        || definition
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
    {
        return Err(AgentRegistryError::Invalid {
            name: definition.name.clone(),
        });
    }
    if !(1..=256).contains(&definition.max_turns) || definition.tools.len() > 128 {
        return Err(AgentRegistryError::Invalid {
            name: definition.name.clone(),
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum AgentRegistryError {
    #[error("invalid agent definition `{name}`")]
    Invalid { name: String },
    #[error("agent `{name}` is already registered")]
    Duplicate { name: String },
    #[error("unknown agent `{name}`")]
    Unknown { name: String },
    #[error("agent `{agent}` selects unknown model alias `{model}`")]
    UnknownModelAlias { agent: String, model: String },
    #[error("agent `{agent}` selects unavailable tool `{tool}`")]
    UnknownTool { agent: String, tool: String },
    #[error(transparent)]
    Discovery(#[from] ExtensionDiscoveryError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use tempfile::TempDir;

    use super::compose_agent_registry;
    use crate::{AgentPermissionMode, ExtensionCatalog, ExtensionDiscoveryConfig};

    #[test]
    fn builtins_and_declarative_agents_share_registry_and_declarative_can_shadow() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let path = project.join(".agents/agents/explore.md");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(
            path,
            "---\nname: explore\ndescription: local explorer\nmodel: fast\ntools: [read]\npermission-mode: discuss\nmax-turns: 4\n---\nLocal prompt.",
        )
        .expect("agent");
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
        );
        let registry = compose_agent_registry(&catalog).expect("registry");

        assert_eq!(registry.definitions().len(), 3);
        let loaded = registry.load("explore").expect("explore");
        assert_eq!(loaded.description, "local explorer");
        assert_eq!(loaded.permission_mode, AgentPermissionMode::Discuss);
        assert_eq!(loaded.system_prompt, "Local prompt.");
    }

    #[test]
    fn builtins_inherit_model_and_explicit_declarative_aliases_are_validated() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let path = project.join(".agents/agents/custom.md");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(
            path,
            "---\nname: custom\ndescription: custom\nmodel: explicit-model\ntools: [read]\npermission-mode: discuss\n---\nCustom prompt.",
        )
        .expect("agent");
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
        );
        let registry = compose_agent_registry(&catalog).expect("registry");

        assert_eq!(registry.load("general").expect("general").model, None);

        let error = registry
            .validate_model_aliases(["default"])
            .expect_err("explicit model is not configured");

        assert!(
            error
                .to_string()
                .contains("unknown model alias `explicit-model`")
        );
    }

    #[test]
    fn declarative_agents_accept_exact_mcp_tools_and_reject_wildcards() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let path = project.join(".agents/agents/mcp-reader.md");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        let write_agent = |tool: &str| {
            std::fs::write(
                &path,
                format!(
                    "---\nname: mcp-reader\ndescription: exact remote reader\nmodel: fast\ntools: [{tool}]\npermission-mode: execute\nmax-turns: 4\n---\nUse only the selected MCP tool."
                ),
            )
            .expect("agent");
        };

        write_agent("mcp:github/get_issue");
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
        );
        let mut registry = compose_agent_registry(&catalog).expect("registry");
        registry
            .resolve_tool_names(std::iter::empty())
            .expect("deferred MCP tool is syntactically resolvable");
        assert_eq!(
            registry.load("mcp-reader").expect("agent").tools,
            ["mcp:github/get_issue"]
        );

        write_agent("tool_search");
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
        );
        let mut registry = compose_agent_registry(&catalog).expect("registry");
        assert!(
            registry
                .resolve_tool_names(["tool_search".to_owned(), "mcp_call".to_owned()])
                .is_err(),
            "declarative children must use exact virtual MCP entries"
        );

        write_agent("mcp:github/*");
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
        );
        assert!(catalog.agent("mcp-reader").is_none());
        assert!(
            catalog.diagnostics()[0]
                .message()
                .contains("exact mcp:<server>/<tool>")
        );
    }
}
