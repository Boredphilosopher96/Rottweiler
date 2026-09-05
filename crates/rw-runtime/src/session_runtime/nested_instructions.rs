use super::initial_memory::redact_initial_memory_frame;
use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_core::AgentLoopError;
use rw_core::ModelDriver;
use rw_core::load_nested_instruction_stack;
use rw_ext::HookDirective;
use rw_ext::HookDispatcher;
use rw_ext::HookError;
use rw_ext::HookEvent;
use rw_ext::HookFailurePolicy;
use rw_ext::HookHandler;
use rw_ext::HookInvocation;
use rw_ext::HookRegistration;
use rw_providers::BoxEventStream;
use rw_providers::FixtureRedactor;
use rw_providers::ProviderRequest;
use rw_tools::ToolBehavior;
use rw_tools::ToolError;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::Role;
use rw_types::Turn;
use rw_types::config::ThinkingLevel;
use std::collections::BTreeSet;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::Weak;

/// Adds nested `AGENTS.md` layers after completed, committed file-tool
/// interactions without mutating the actor's persisted initial prefix.
pub(super) struct NestedInstructionsModel {
    pub(super) inner: Arc<dyn ModelDriver>,
    pub(super) tools: Arc<OnceLock<Weak<ToolRegistry>>>,
    pub(super) workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    pub(super) active_sources: Arc<RwLock<BTreeSet<PathBuf>>>,
    pub(super) memory_redactor: FixtureRedactor,
}

impl NestedInstructionsModel {
    pub(super) fn augment(
        &self,
        request: &mut ProviderRequest,
    ) -> std::result::Result<(), AgentLoopError> {
        for turn in &mut request.turns {
            for block in &mut turn.blocks {
                let Block::Text { text } = block else {
                    continue;
                };
                if let Some(redacted) = redact_initial_memory_frame(text, &self.memory_redactor)? {
                    *text = redacted;
                }
            }
        }
        let roots = self
            .workspace_roots
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let tools = self.tools.get().and_then(Weak::upgrade).ok_or_else(|| {
            AgentLoopError::InvalidConfiguration(
                "session tool registry is not available for model use".to_owned(),
            )
        })?;
        let touched =
            completed_file_tool_paths(&request.turns, &roots, &tools).map_err(|error| {
                AgentLoopError::ToolContext(format!(
                    "completed tool path semantics could not be resolved: {error}"
                ))
            })?;
        if touched.is_empty() {
            return Ok(());
        }
        let stack = load_nested_instruction_stack(&roots, &touched).map_err(|error| {
            AgentLoopError::InvalidConfiguration(format!(
                "nested project instructions could not load: {error}"
            ))
        })?;
        {
            let mut active = self
                .active_sources
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.extend(
                stack
                    .layers()
                    .iter()
                    .map(|layer| layer.source().to_path_buf()),
            );
        }
        let additions = stack
            .as_system_turns()
            .into_iter()
            .filter(|turn| !request.turns.contains(turn))
            .collect::<Vec<_>>();
        if additions.is_empty() {
            return Ok(());
        }
        let insertion = request.cache_hint.map_or_else(
            || {
                request
                    .turns
                    .iter()
                    .take_while(|turn| turn.role == Role::System)
                    .count()
            },
            |hint| usize::try_from(hint.stable_prefix_turns).unwrap_or(usize::MAX),
        );
        let insertion = insertion.min(request.turns.len());
        request.turns.splice(insertion..insertion, additions);
        Ok(())
    }
}

#[async_trait]
impl ModelDriver for NestedInstructionsModel {
    async fn settle_effects(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        self.inner.settle_effects().await
    }

    fn stream(
        &self,
        alias: &str,
        mut request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        self.augment(&mut request)?;
        self.inner.stream(alias, request, invocation)
    }

    fn stream_for_provider(
        &self,
        alias: &str,
        provider: Option<&str>,
        mut request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        self.augment(&mut request)?;
        self.inner
            .stream_for_provider(alias, provider, request, invocation)
    }

    fn context_metadata(&self, alias: &str) -> rw_core::ModelContextMetadata {
        self.inner.context_metadata(alias)
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        self.inner.has_model_alias(alias)
    }

    fn title_model_alias(&self) -> Option<String> {
        self.inner.title_model_alias()
    }

    async fn prepare_model(&self, alias: &str) -> std::result::Result<(), AgentLoopError> {
        self.inner.prepare_model(alias).await
    }

    fn commit_prepared_model(&self, alias: &str) {
        self.inner.commit_prepared_model(alias);
    }

    fn discard_prepared_model(&self, alias: &str) {
        self.inner.discard_prepared_model(alias);
    }

    async fn activate_provider(
        &self,
        provider: &str,
        selected_model: Option<&str>,
    ) -> std::result::Result<(), AgentLoopError> {
        self.inner.activate_provider(provider, selected_model).await
    }

    fn thinking_for_model(&self, model: &str, fallback: ThinkingLevel) -> ThinkingLevel {
        self.inner.thinking_for_model(model, fallback)
    }

    fn has_provider_for_alias(&self, alias: &str, provider: &str) -> bool {
        self.inner.has_provider_for_alias(alias, provider)
    }

    fn supports_vision(&self, alias: &str) -> bool {
        self.inner.supports_vision(alias)
    }

    fn compaction_config(&self) -> rw_core::CompactionConfig {
        self.inner.compaction_config()
    }

    fn budget_config(&self) -> rw_core::BudgetConfig {
        self.inner.budget_config()
    }

    fn cost(&self, alias: &str, usage: rw_core::ModelTokenUsage) -> rw_core::Cost {
        self.inner.cost(alias, usage)
    }

    fn cost_for_reported_model(
        &self,
        alias: &str,
        reported_model: Option<&str>,
        usage: rw_core::ModelTokenUsage,
    ) -> rw_core::Cost {
        self.inner
            .cost_for_reported_model(alias, reported_model, usage)
    }

    fn cost_for_route(
        &self,
        alias: &str,
        route: Option<&str>,
        reported_model: Option<&str>,
        usage: rw_core::ModelTokenUsage,
    ) -> rw_core::Cost {
        self.inner
            .cost_for_route(alias, route, reported_model, usage)
    }
}

pub(super) struct NestedInstructionsPreToolGuard {
    pub(super) tools: Arc<ToolRegistry>,
    pub(super) workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    pub(super) active_sources: Arc<RwLock<BTreeSet<PathBuf>>>,
}

#[async_trait]
impl HookHandler for NestedInstructionsPreToolGuard {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(
        &self,
        invocation: HookInvocation<'_>,
    ) -> std::result::Result<HookDirective, HookError> {
        if invocation.event() != HookEvent::PreTool {
            return Ok(HookDirective::Continue);
        }
        let payload = invocation.payload();
        let Some(tool_name) = payload.get("name").and_then(serde_json::Value::as_str) else {
            return Ok(HookDirective::Continue);
        };
        let arguments = payload
            .get("arguments")
            .ok_or_else(|| HookError::new("tool_semantics", "tool arguments are missing"))?;
        let semantics = self
            .tools
            .invocation_semantics(tool_name, arguments)
            .map_err(|error| HookError::new("tool_semantics", error.to_string()))?
            .ok_or_else(|| HookError::new("tool_semantics", "tool is not registered"))?;
        if semantics.behavior != ToolBehavior::FileMutation {
            return Ok(HookDirective::Continue);
        }
        let roots = self
            .workspace_roots
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let touched = semantics
            .workspace_paths
            .iter()
            .map(|path| resolve_instruction_tool_path(&roots, path))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                HookError::new(
                    "tool_semantics",
                    "registered file mutation path is outside the workspace",
                )
            })?;
        if touched.is_empty() {
            return Err(HookError::new(
                "tool_semantics",
                "registered file mutation did not declare a workspace path",
            ));
        }
        let stack =
            tokio::task::spawn_blocking(move || load_nested_instruction_stack(&roots, &touched))
                .await
                .map_err(|_| {
                    HookError::new(
                        "nested_instruction_discovery",
                        "nested project instruction discovery did not complete",
                    )
                })?
                .map_err(|error| {
                    HookError::new("nested_instruction_discovery", error.to_string())
                })?;
        let active = self
            .active_sources
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unseen = stack
            .layers()
            .iter()
            .map(|layer| layer.source().to_path_buf())
            .filter(|source| !active.contains(source))
            .collect::<Vec<_>>();
        if unseen.is_empty() {
            return Ok(HookDirective::Continue);
        }
        Ok(HookDirective::Block {
            message: format!(
                "Nested project instructions apply to this path and must be loaded before mutation. Retry the tool after guidance is added. sources={}",
                unseen
                    .iter()
                    .map(|source| source.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        })
    }
}

pub(super) fn register_nested_instruction_guard(
    dispatcher: &mut HookDispatcher,
    tools: Arc<ToolRegistry>,
    workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    active_sources: Arc<RwLock<BTreeSet<PathBuf>>>,
) -> Result<()> {
    let applicable_tools = tools
        .names_with_behavior(ToolBehavior::FileMutation)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    dispatcher
        .register(
            HookRegistration::new("builtin.nested_instructions", HookEvent::PreTool)
                .with_priority(i32::MIN.saturating_add(1))
                .with_failure_policy(HookFailurePolicy::FailClosed)
                .with_applicable_tools(applicable_tools)
                .with_timeout(std::time::Duration::from_secs(5)),
            NestedInstructionsPreToolGuard {
                tools,
                workspace_roots,
                active_sources,
            },
        )
        .map_err(|error| miette!("nested instruction guard could not register: {error}"))
}

pub(super) fn completed_file_tool_paths(
    turns: &[Turn],
    roots: &[PathBuf],
    tools: &ToolRegistry,
) -> Result<Vec<PathBuf>, ToolError> {
    let completed = turns
        .iter()
        .flat_map(|turn| &turn.blocks)
        .filter_map(|block| match block {
            Block::ToolResult { id, .. } => Some(id.0.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    turns
        .iter()
        .flat_map(|turn| &turn.blocks)
        .filter_map(|block| match block {
            Block::ToolCall { id, name, args } if completed.contains(&id.0) => Some((name, args)),
            _ => None,
        })
        .try_fold(BTreeSet::new(), |mut paths, (name, args)| {
            let semantics = tools.invocation_semantics(name, args)?.ok_or_else(|| {
                ToolError::InvalidInput(format!("unknown historical tool: {name}"))
            })?;
            paths.extend(
                semantics
                    .workspace_paths
                    .iter()
                    .filter_map(|path| resolve_instruction_tool_path(roots, path)),
            );
            Ok(paths)
        })
        .map(|paths| paths.into_iter().collect())
}

pub(super) fn resolve_instruction_tool_path(roots: &[PathBuf], supplied: &Path) -> Option<PathBuf> {
    if supplied.is_absolute() {
        return roots
            .iter()
            .any(|root| supplied.starts_with(root))
            .then(|| supplied.to_path_buf());
    }
    if supplied.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    let mut components = supplied.components();
    if components
        .next()
        .is_some_and(|component| matches!(component, Component::Normal(name) if name == "@root"))
    {
        let Component::Normal(index) = components.next()? else {
            return None;
        };
        let index = index
            .to_str()?
            .parse::<usize>()
            .ok()
            .filter(|index| *index > 0)?;
        let root = roots.get(index)?;
        return Some(components.fold(root.clone(), |path, component| {
            path.join(component.as_os_str())
        }));
    }
    roots.first().map(|root| root.join(supplied))
}
