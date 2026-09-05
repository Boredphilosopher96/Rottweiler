use super::command_execution::PrivateScratch;
use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_tools::CodeIntelligence;
use rw_tools::CodeIntelligenceProvider;
use rw_tools::Diagnostic;
use rw_tools::IntelligenceBackend;
use rw_tools::IntelligenceResult;
use rw_tools::Location;
use rw_tools::LspConfig;
use rw_tools::Position;
use rw_tools::RenameResult;
use rw_tools::SandboxedLspSpawner;
use rw_tools::SymbolsTool;
use rw_tools::Tool;
use rw_tools::ToolContext;
use rw_tools::ToolDescriptor;
use rw_tools::ToolError;
use rw_tools::ToolLimits;
use rw_tools::ToolResult;
use rw_tools::WorkspaceSymbolIndex;
use rw_tools::WorkspaceUriMapper;
use rw_tools::discover_sandboxed_lsp_servers;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct LazySymbolsTool {
    pub(super) inner: SymbolsTool,
    pub(super) index: Arc<WorkspaceSymbolIndex>,
}

pub(super) struct MultiRootCodeIntelligence {
    pub(super) providers: Vec<Arc<CodeIntelligence>>,
    pub(super) symbols: Arc<WorkspaceSymbolIndex>,
    pub(super) _scratch: PrivateScratch,
}

pub(super) fn lsp_servers_for_root(
    servers: &[rw_tools::LspServerConfig],
    trusted: bool,
) -> Vec<rw_tools::LspServerConfig> {
    if trusted {
        servers.to_vec()
    } else {
        Vec::new()
    }
}

impl MultiRootCodeIntelligence {
    pub(super) fn new(
        roots: &[PathBuf],
        trusted_roots: &[bool],
        symbols: Arc<WorkspaceSymbolIndex>,
        offline: bool,
    ) -> Result<Self> {
        let indexes = symbols.root_indexes();
        if roots.len() != indexes.len() || roots.len() != trusted_roots.len() {
            return Err(miette!("code-intelligence root mapping is inconsistent"));
        }
        let servers = if offline {
            Vec::new()
        } else {
            discover_sandboxed_lsp_servers(roots)
        };
        let scratch = PrivateScratch::create("lsp")?;
        let helper = std::env::current_exe()
            .map_err(|error| miette!("LSP sandbox helper could not resolve: {error}"))?;
        let spawner = Arc::new(
            SandboxedLspSpawner::new(roots, scratch.path(), helper)
                .map_err(|error| miette!("LSP sandbox could not start: {error}"))?,
        );
        let uri_mapper = Arc::new(
            WorkspaceUriMapper::new(roots)
                .map_err(|error| miette!("LSP workspace mapping could not start: {error}"))?,
        );
        let providers = roots
            .iter()
            .zip(indexes)
            .zip(trusted_roots)
            .map(|((root, index), trusted)| {
                let config = LspConfig {
                    servers: lsp_servers_for_root(&servers, *trusted),
                    ..LspConfig::default()
                };
                CodeIntelligence::new_with_uri_mapper(
                    root,
                    Arc::clone(index),
                    config,
                    spawner.clone(),
                    Arc::clone(&uri_mapper),
                )
                .map(Arc::new)
                .map_err(|error| miette!("code-intelligence workspace could not start: {error}"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            providers,
            symbols,
            _scratch: scratch,
        })
    }

    pub(super) fn route(&self, path: &Path) -> Option<(usize, PathBuf)> {
        let mut components = path.components();
        let first = components.next()?;
        if matches!(first, std::path::Component::Normal(value) if value == "@root") {
            let index = match components.next()? {
                std::path::Component::Normal(value) => value.to_str()?.parse::<usize>().ok()?,
                _ => return None,
            };
            let relative = components.collect::<PathBuf>();
            (index > 0 && index < self.providers.len() && !relative.as_os_str().is_empty())
                .then_some((index, relative))
        } else {
            Some((0, path.to_path_buf()))
        }
    }

    pub(super) async fn ensure_indexed(&self) -> std::result::Result<(), String> {
        let symbols = Arc::clone(&self.symbols);
        tokio::task::spawn_blocking(move || symbols.ensure_current())
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }

    pub(super) fn virtualize_path(root_index: usize, path: &mut PathBuf) {
        if root_index == 0
            || path.components().next().is_some_and(
                |component| matches!(component, std::path::Component::Normal(value) if value == "@root"),
            )
        {
            return;
        }
        let current = path.clone();
        *path = PathBuf::from("@root")
            .join(root_index.to_string())
            .join(current);
    }
}

#[async_trait]
impl CodeIntelligenceProvider for MultiRootCodeIntelligence {
    async fn diagnostics(&self, path: &Path, source: &str) -> IntelligenceResult<Diagnostic> {
        let Some((root_index, relative)) = self.route(path) else {
            return IntelligenceResult {
                backend: IntelligenceBackend::TreeSitter,
                items: Vec::new(),
                note: Some("invalid workspace root path".to_owned()),
            };
        };
        let mut result = self.providers[root_index]
            .diagnostics(relative, source)
            .await;
        for diagnostic in &mut result.items {
            Self::virtualize_path(root_index, &mut diagnostic.path);
        }
        result
    }

    async fn definition(&self, path: &Path, position: Position) -> IntelligenceResult<Location> {
        self.locations(path, position, false).await
    }

    async fn references(&self, path: &Path, position: Position) -> IntelligenceResult<Location> {
        self.locations(path, position, true).await
    }

    async fn rename(&self, path: &Path, position: Position, new_name: &str) -> RenameResult {
        let Some((root_index, relative)) = self.route(path) else {
            return RenameResult {
                backend: IntelligenceBackend::TreeSitter,
                edits: Vec::new(),
                note: Some("invalid workspace root path".to_owned()),
            };
        };
        let mut result = self.providers[root_index]
            .rename(relative, position, new_name)
            .await;
        for edit in &mut result.edits {
            Self::virtualize_path(root_index, &mut edit.path);
        }
        result
    }

    async fn active_lsp_servers(&self) -> Vec<String> {
        let mut names = Vec::new();
        for provider in &self.providers {
            names.extend(provider.active_server_names().await);
        }
        names.sort();
        names.dedup();
        names
    }
}

impl MultiRootCodeIntelligence {
    pub(super) async fn locations(
        &self,
        path: &Path,
        position: Position,
        references: bool,
    ) -> IntelligenceResult<Location> {
        let Some((root_index, relative)) = self.route(path) else {
            return IntelligenceResult {
                backend: IntelligenceBackend::TreeSitter,
                items: Vec::new(),
                note: Some("invalid workspace root path".to_owned()),
            };
        };
        let mut result = if references {
            self.providers[root_index]
                .references(&relative, position)
                .await
        } else {
            self.providers[root_index]
                .definition(&relative, position)
                .await
        };
        let indexing_note = if result.backend == IntelligenceBackend::TreeSitter {
            let note = self.ensure_indexed().await.err();
            result = if references {
                self.providers[root_index]
                    .references(relative, position)
                    .await
            } else {
                self.providers[root_index]
                    .definition(relative, position)
                    .await
            };
            note
        } else {
            None
        };
        for location in &mut result.items {
            Self::virtualize_path(root_index, &mut location.path);
        }
        if result.note.is_none() {
            result.note = indexing_note;
        }
        result
    }
}

impl LazySymbolsTool {
    pub(super) fn new(index: Arc<WorkspaceSymbolIndex>, limits: ToolLimits) -> Self {
        Self {
            inner: SymbolsTool::new(Arc::clone(&index), limits),
            index,
        }
    }
}

#[async_trait]
impl Tool for LazySymbolsTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        self.inner.descriptor()
    }

    async fn execute(
        &self,
        context: &ToolContext,
        input: serde_json::Value,
    ) -> std::result::Result<ToolResult, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let index = Arc::clone(&self.index);
        tokio::task::spawn_blocking(move || index.ensure_current())
            .await
            .map_err(|error| ToolError::Intelligence(error.to_string()))?
            .map_err(|error| ToolError::Intelligence(error.to_string()))?;
        self.inner.execute(context, input).await
    }
}
