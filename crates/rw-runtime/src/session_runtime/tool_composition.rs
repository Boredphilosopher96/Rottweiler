use super::code_intelligence::LazySymbolsTool;
use super::code_intelligence::MultiRootCodeIntelligence;
use super::command_execution::CommandFixtureMode;
use super::command_execution::DeferredCommandExecutor;
use super::command_execution::build_command_executor;
use super::command_execution::build_read_only_hook_executor;
use super::credential_resolution::DeferredToolProxy;
use super::credential_resolution::DeferredWebSearchHeaders;
use super::credential_resolution::ResolvedToolProxy;
use super::deferred_network::DeferredConfiguredWebSearcher;
use super::deferred_network::DeferredPolicyWebFetcher;
use super::deferred_network::configured_web_searcher;
use super::native_search::RuntimeWebSearcher;
use super::web_fetch::OfflineWebFetcher;
use super::web_fetch::PolicyWebFetcher;
use miette::Result;
use miette::miette;
use rw_store::trust::FolderTrustStore;
use rw_tools::AskUserTool;
use rw_tools::BackgroundKillTool;
use rw_tools::BackgroundOutputTool;
use rw_tools::BackgroundProcessLimits;
use rw_tools::BackgroundProcessManager;
use rw_tools::BackgroundStatusTool;
use rw_tools::BashTool;
use rw_tools::CodeIntelligenceProvider;
use rw_tools::CommandExecutor;
use rw_tools::CommandFixtureRedactor;
use rw_tools::CommandSafetyClassifier;
use rw_tools::DefinitionTool;
use rw_tools::DiagnosticsTool;
use rw_tools::EditTool;
use rw_tools::ExecutionLease;
use rw_tools::GlobTool;
use rw_tools::GrepTool;
use rw_tools::LsTool;
use rw_tools::MultiEditTool;
use rw_tools::QuestionAsker;
use rw_tools::ReadTool;
use rw_tools::ReferencesTool;
use rw_tools::RenameTool;
use rw_tools::SubmitPlanTool;
use rw_tools::TodoTool;
use rw_tools::Tool;
use rw_tools::ToolLimits;
use rw_tools::ToolRegistry;
use rw_tools::WebFetchTool;
use rw_tools::WebFetcher;
use rw_tools::WebSearchTool;
use rw_tools::WebSearcher;
use rw_tools::WriteTool;
use rw_types::config::WebSearchConfig;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct BuildToolsInput<'a> {
    pub(super) index_pool: Arc<rw_tools::WorkspaceIndexPool>,
    pub(super) workspace_roots: &'a [PathBuf],
    pub(super) trusted_lsp_roots: &'a [bool],
    pub(super) question_asker: Arc<dyn QuestionAsker>,
    pub(super) offline: bool,
    pub(super) global_proxy: Option<&'a ResolvedToolProxy>,
    pub(super) deferred_global_proxy: Option<DeferredToolProxy>,
    pub(super) command_fixture_mode: CommandFixtureMode,
    pub(super) execution_lease: Arc<ExecutionLease>,
    pub(super) command_safety: &'a Arc<CommandSafetyClassifier>,
    pub(super) websearch_config: &'a WebSearchConfig,
    pub(super) websearch_headers: &'a BTreeMap<String, String>,
    pub(super) deferred_websearch_headers: Option<DeferredWebSearchHeaders>,
    pub(super) native_websearch_possible: bool,
    pub(super) background_redactor: Arc<dyn CommandFixtureRedactor>,
    pub(super) background_manager: Option<Arc<BackgroundProcessManager>>,
}

pub(super) fn trusted_lsp_roots(
    roots: &[PathBuf],
    trust_store_path: &Path,
    dangerously_trust: bool,
) -> Result<Vec<bool>> {
    if dangerously_trust {
        return Ok(vec![true; roots.len()]);
    }
    let store = FolderTrustStore::new(trust_store_path.to_path_buf());
    roots
        .iter()
        .map(|root| {
            store
                .assess(root)
                .map(|assessment| assessment.project_execution_enabled())
                .map_err(|error| miette!("workspace LSP trust could not be assessed: {error}"))
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
pub(super) fn build_tools(input: BuildToolsInput<'_>) -> Result<BuiltTools> {
    let BuildToolsInput {
        index_pool,
        workspace_roots,
        trusted_lsp_roots,
        question_asker,
        offline,
        global_proxy,
        deferred_global_proxy,
        command_fixture_mode,
        execution_lease,
        command_safety,
        websearch_config,
        websearch_headers,
        deferred_websearch_headers,
        native_websearch_possible,
        background_redactor,
        background_manager,
    } = input;
    let workspace = workspace_roots
        .first()
        .ok_or_else(|| miette!("tool composition requires a primary workspace"))?;
    let symbols = Arc::new(
        index_pool
            .workspace(workspace_roots, trusted_lsp_roots)
            .map_err(|error| miette!("symbol index could not start: {error}"))?,
    );
    let limits = ToolLimits::default();
    let todo = Arc::new(TodoTool::new(limits));
    let web_fetcher: Arc<dyn WebFetcher> = if offline {
        Arc::new(OfflineWebFetcher)
    } else if let Some(proxy) = deferred_global_proxy.clone() {
        Arc::new(DeferredPolicyWebFetcher::new(proxy))
    } else {
        Arc::new(PolicyWebFetcher::new(false, global_proxy.cloned()))
    };
    let websearch_fixture_mode = command_fixture_mode.clone();
    let hook_fixture_mode = command_fixture_mode.clone();
    let command_executor: Arc<dyn CommandExecutor> = if let Some(proxy) = deferred_global_proxy {
        Arc::new(DeferredCommandExecutor::new(
            workspace_roots,
            workspace,
            command_fixture_mode,
            Arc::clone(&execution_lease),
            Arc::clone(command_safety),
            proxy,
        ))
    } else {
        build_command_executor(
            workspace_roots,
            workspace,
            command_fixture_mode,
            &execution_lease,
            command_safety,
            global_proxy,
        )?
    };
    let background = background_manager.unwrap_or_else(|| {
        Arc::new(BackgroundProcessManager::new(
            background_redactor,
            BackgroundProcessLimits::default(),
        ))
    });
    let (read_only_hook_executor, read_only_hook_scratch) =
        build_read_only_hook_executor(hook_fixture_mode, &execution_lease, command_safety)?;
    let bash: Arc<dyn Tool> = Arc::new(
        BashTool::new(Arc::clone(&command_executor), limits)
            .with_command_safety(Arc::clone(command_safety))
            .with_background_manager(Arc::clone(&background)),
    );
    let code_intelligence: Arc<dyn CodeIntelligenceProvider> =
        Arc::new(MultiRootCodeIntelligence::new(
            workspace_roots,
            trusted_lsp_roots,
            Arc::clone(&symbols),
            offline,
        )?);
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new(limits)),
        Arc::new(WriteTool::new(limits).with_symbol_index(Arc::clone(&symbols))),
        Arc::new(EditTool::new(limits).with_symbol_index(Arc::clone(&symbols))),
        Arc::new(MultiEditTool::new(limits).with_symbol_index(Arc::clone(&symbols))),
        Arc::new(GrepTool::new(limits)),
        Arc::new(GlobTool::new(limits)),
        Arc::new(LsTool::new(limits)),
        bash,
        Arc::new(BackgroundStatusTool::new(Arc::clone(&background))),
        Arc::new(BackgroundOutputTool::new(Arc::clone(&background))),
        Arc::new(BackgroundKillTool::new(Arc::clone(&background))),
        Arc::new(WebFetchTool::new(Arc::clone(&web_fetcher), limits)),
        todo.clone(),
        Arc::new(AskUserTool::new(question_asker, limits)),
        Arc::new(SubmitPlanTool),
        Arc::new(LazySymbolsTool::new(Arc::clone(&symbols), limits)),
        Arc::new(DiagnosticsTool::new(Arc::clone(&code_intelligence), limits)),
        Arc::new(DefinitionTool::new(Arc::clone(&code_intelligence), limits)),
        Arc::new(ReferencesTool::new(Arc::clone(&code_intelligence), limits)),
        Arc::new(RenameTool::new(Arc::clone(&code_intelligence), limits)),
    ];
    let configured_searcher = if let Some(headers) = deferred_websearch_headers {
        Some(Arc::new(DeferredConfiguredWebSearcher::new(
            websearch_config.clone(),
            headers,
            Arc::clone(&web_fetcher),
            limits,
            websearch_fixture_mode.clone(),
        )?) as Arc<dyn WebSearcher>)
    } else {
        configured_web_searcher(
            offline,
            websearch_config,
            websearch_headers,
            &web_fetcher,
            limits,
            &websearch_fixture_mode,
        )?
    };
    let websearch = (configured_searcher.is_some() || native_websearch_possible)
        .then(|| Arc::new(RuntimeWebSearcher::new(configured_searcher)));
    if let Some(searcher) = &websearch {
        tools.push(Arc::new(WebSearchTool::new(searcher.clone(), limits)));
    }
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry
            .register(tool)
            .map_err(|error| miette!("built-in tools could not register: {error}"))?;
    }
    Ok(BuiltTools {
        registry: Arc::new(registry),
        todo,
        command_executor,
        read_only_hook_executor,
        read_only_hook_scratch,
        code_intelligence,
        websearch,
        background,
        _execution_lease: execution_lease,
    })
}

pub(super) fn command_mode_can_open_proxy(mode: &CommandFixtureMode) -> bool {
    matches!(
        mode,
        CommandFixtureMode::Live | CommandFixtureMode::Record { .. }
    )
}

pub(super) struct BuiltTools {
    pub(super) registry: Arc<ToolRegistry>,
    pub(super) todo: Arc<TodoTool>,
    pub(super) command_executor: Arc<dyn CommandExecutor>,
    pub(super) read_only_hook_executor: Arc<dyn CommandExecutor>,
    pub(super) read_only_hook_scratch: PathBuf,
    pub(super) code_intelligence: Arc<dyn CodeIntelligenceProvider>,
    pub(super) websearch: Option<Arc<RuntimeWebSearcher>>,
    pub(super) background: Arc<BackgroundProcessManager>,
    pub(super) _execution_lease: Arc<ExecutionLease>,
}
