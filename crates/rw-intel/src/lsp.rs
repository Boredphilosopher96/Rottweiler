//! A deliberately small, bounded Language Server Protocol client.
//!
//! Servers are optional. Every query falls back to the local syntax index when
//! a server is missing, crashes, times out, or returns malformed protocol data.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::Mutex;
use tokio::time::timeout;
use url::Url;

use crate::{Language, SourceLocation, SymbolIndex, SymbolQuery, SymbolRole};

const MAX_HEADER_BYTES: usize = 8 * 1024;
const DEFAULT_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Zero-based LSP position.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// Half-open LSP source range.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A source location returned by an LSP server or syntax fallback.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

/// Standard LSP diagnostic severity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
    Unknown,
}

/// Bounded, display-safe diagnostic published by a language server.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// One rename edit. Paths are guaranteed to be inside the workspace.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TextEdit {
    pub path: PathBuf,
    pub range: Range,
    pub new_text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntelligenceBackend {
    Lsp,
    TreeSitter,
}

/// Query output with an explicit degradation signal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IntelligenceResult<T> {
    pub backend: IntelligenceBackend,
    pub items: Vec<T>,
    pub note: Option<String>,
}

/// Rename is never guessed by the syntax fallback.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RenameResult {
    pub backend: IntelligenceBackend,
    pub edits: Vec<TextEdit>,
    pub note: Option<String>,
}

/// One language-server launch specification. Commands are spawned directly,
/// never interpreted by a shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspServerConfig {
    pub language: Language,
    pub command: PathBuf,
    pub args: Vec<String>,
}

/// Kill boundary for an injected language-server process. `rw-intel` owns no
/// ambient process-launch authority; hosts must provide a sandboxed spawner.
#[async_trait]
pub trait LspProcessHandle: Send {
    async fn kill(&mut self) -> io::Result<()>;
}

/// A child process with its protocol pipes separated from its lifecycle
/// handle. All three values must refer to the same already-launched process.
pub struct SpawnedLspProcess {
    pub handle: Box<dyn LspProcessHandle>,
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    pub stdout: Pin<Box<dyn tokio::io::AsyncRead + Send>>,
}

/// Sole authority boundary through which `rw-intel` may obtain an LSP child.
#[async_trait]
pub trait LspProcessSpawner: Send + Sync {
    async fn spawn(
        &self,
        workspace: &Path,
        server: &LspServerConfig,
    ) -> Result<SpawnedLspProcess, LspError>;
}

/// Resource and restart bounds for the optional LSP tier.
#[derive(Clone, Debug)]
pub struct LspConfig {
    pub servers: Vec<LspServerConfig>,
    pub request_timeout: Duration,
    pub max_message_bytes: usize,
    pub max_restarts: usize,
    pub max_results: usize,
    pub max_diagnostics: usize,
    pub max_documents: usize,
    pub notification_drain_timeout: Duration,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            request_timeout: Duration::from_secs(10),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_restarts: 1,
            max_results: 1_000,
            max_diagnostics: 2_000,
            max_documents: 512,
            notification_drain_timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Error)]
pub enum LspError {
    #[error("language server is unavailable")]
    Unavailable,
    #[error("language server request timed out")]
    Timeout,
    #[error("language server protocol error: {0}")]
    Protocol(&'static str),
    #[error("language server I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("language server rejected the request: {0}")]
    Response(String),
    #[error("path is outside the language-server workspace")]
    PathEscape,
}

struct ServerSlot {
    spec: LspServerConfig,
    client: Option<LspClient>,
    restarts: usize,
}

struct LspClient {
    child: Box<dyn LspProcessHandle>,
    stdin: Pin<Box<dyn AsyncWrite + Send>>,
    stdout: BufReader<Pin<Box<dyn tokio::io::AsyncRead + Send>>>,
    next_id: u64,
    diagnostics: BTreeMap<PathBuf, Vec<Diagnostic>>,
    document_versions: BTreeMap<String, i32>,
}

/// Shared canonical mapping for every active workspace root. LSP responses may
/// legally target another workspace folder, so confinement must cover the
/// complete active root set rather than only the server's primary root.
pub struct WorkspaceUriMapper {
    roots: Vec<PathBuf>,
}

impl WorkspaceUriMapper {
    /// Canonicalize the complete active workspace root set.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when a root cannot be canonicalized, or an
    /// unavailable error when no roots are supplied.
    pub fn new(roots: &[PathBuf]) -> Result<Self, LspError> {
        let roots = roots
            .iter()
            .map(std::fs::canonicalize)
            .collect::<io::Result<Vec<_>>>()?;
        if roots.is_empty() {
            return Err(LspError::Unavailable);
        }
        Ok(Self { roots })
    }

    fn uri_path(&self, uri: &str) -> Option<PathBuf> {
        let path = Url::parse(uri).ok()?.to_file_path().ok()?;
        self.absolute_path(&path)
    }

    fn absolute_path(&self, path: &Path) -> Option<PathBuf> {
        let canonical = std::fs::canonicalize(path).ok()?;
        let (index, root) = self
            .roots
            .iter()
            .enumerate()
            .filter(|(_, root)| canonical.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())?;
        let relative = canonical.strip_prefix(root).ok()?;
        if index == 0 {
            Some(relative.to_path_buf())
        } else {
            Some(
                PathBuf::from("@root")
                    .join(index.to_string())
                    .join(relative),
            )
        }
    }
}

/// Workspace code-intelligence facade. LSP state is isolated per workspace and
/// all methods degrade without making the syntax index unavailable.
pub struct CodeIntelligence {
    root: PathBuf,
    syntax: Arc<SymbolIndex>,
    config: LspConfig,
    spawner: Arc<dyn LspProcessSpawner>,
    uri_mapper: Arc<WorkspaceUriMapper>,
    servers: BTreeMap<Language, Mutex<ServerSlot>>,
}

impl CodeIntelligence {
    /// Create a facade without starting a child process. Servers start lazily on
    /// the first query for their language.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the workspace root cannot be canonicalized.
    pub fn new(
        root: impl AsRef<Path>,
        syntax: Arc<SymbolIndex>,
        config: LspConfig,
        spawner: Arc<dyn LspProcessSpawner>,
    ) -> Result<Self, LspError> {
        let root = std::fs::canonicalize(root).map_err(LspError::Io)?;
        let uri_mapper = Arc::new(WorkspaceUriMapper::new(std::slice::from_ref(&root))?);
        Ok(Self::new_canonical(
            root, syntax, config, spawner, uri_mapper,
        ))
    }

    /// Create a lazy client using a mapper shared by all active roots.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the server workspace cannot be canonicalized.
    pub fn new_with_uri_mapper(
        root: impl AsRef<Path>,
        syntax: Arc<SymbolIndex>,
        config: LspConfig,
        spawner: Arc<dyn LspProcessSpawner>,
        uri_mapper: Arc<WorkspaceUriMapper>,
    ) -> Result<Self, LspError> {
        let root = std::fs::canonicalize(root).map_err(LspError::Io)?;
        Ok(Self::new_canonical(
            root, syntax, config, spawner, uri_mapper,
        ))
    }

    fn new_canonical(
        root: PathBuf,
        syntax: Arc<SymbolIndex>,
        config: LspConfig,
        spawner: Arc<dyn LspProcessSpawner>,
        uri_mapper: Arc<WorkspaceUriMapper>,
    ) -> Self {
        let servers = config
            .servers
            .iter()
            .cloned()
            .map(|spec| {
                (
                    spec.language,
                    Mutex::new(ServerSlot {
                        spec,
                        client: None,
                        restarts: 0,
                    }),
                )
            })
            .collect();
        Self {
            root,
            syntax,
            config,
            spawner,
            uri_mapper,
            servers,
        }
    }

    #[must_use]
    pub fn detected_languages(&self) -> Vec<Language> {
        self.servers.keys().copied().collect()
    }

    /// Notify a server of current source and return diagnostics available for
    /// that document. Failure returns an empty syntax-tier result.
    pub async fn diagnostics(
        &self,
        path: impl AsRef<Path>,
        source: &str,
    ) -> IntelligenceResult<Diagnostic> {
        let path = path.as_ref();
        let Some(language) = Language::for_path(path) else {
            return fallback(Vec::new(), "no language server for this file type");
        };
        let Ok(relative) = self.relative_path(path) else {
            return fallback(Vec::new(), "path is outside the workspace");
        };
        let uri = self
            .uri(&relative)
            .map_or_else(|_| String::new(), |uri| uri.to_string());
        let cache_path = self
            .uri_mapper
            .absolute_path(&self.root.join(&relative))
            .unwrap_or_else(|| relative.clone());
        if self
            .update_document(language, &uri, &cache_path, source)
            .await
            .is_err()
        {
            return fallback(Vec::new(), "language server unavailable");
        }
        let pull = self
            .request(
                language,
                "textDocument/diagnostic",
                json!({"textDocument": {"uri": uri}}),
            )
            .await;
        if pull.is_err() {
            self.drain_notifications(language, &cache_path).await;
        }
        let mut diagnostics = match pull {
            Ok(value) => {
                let path = self
                    .uri_mapper
                    .absolute_path(&self.root.join(&relative))
                    .unwrap_or(relative.clone());
                parse_pull_diagnostics(&path, &value)
            }
            Err(_) => self.cached_diagnostics(language, &relative).await,
        };
        diagnostics.truncate(self.config.max_diagnostics);
        IntelligenceResult {
            backend: IntelligenceBackend::Lsp,
            items: diagnostics,
            note: None,
        }
    }

    pub async fn definition(
        &self,
        path: impl AsRef<Path>,
        position: Position,
    ) -> IntelligenceResult<Location> {
        self.location_query(
            path.as_ref(),
            position,
            "textDocument/definition",
            SymbolRole::Definition,
        )
        .await
    }

    pub async fn references(
        &self,
        path: impl AsRef<Path>,
        position: Position,
    ) -> IntelligenceResult<Location> {
        self.location_query(
            path.as_ref(),
            position,
            "textDocument/references",
            SymbolRole::Reference,
        )
        .await
    }

    /// Request a rename edit set. An unavailable server produces no edits;
    /// syntax matches are not safe enough to rewrite automatically.
    pub async fn rename(
        &self,
        path: impl AsRef<Path>,
        position: Position,
        new_name: &str,
    ) -> RenameResult {
        if new_name.is_empty() || new_name.len() > 512 || new_name.chars().any(char::is_whitespace)
        {
            return RenameResult {
                backend: IntelligenceBackend::TreeSitter,
                edits: Vec::new(),
                note: Some("invalid rename target".to_owned()),
            };
        }
        let path = path.as_ref();
        let Some(language) = Language::for_path(path) else {
            return rename_fallback("no language server for this file type");
        };
        let Ok(relative) = self.relative_path(path) else {
            return rename_fallback("path is outside the workspace");
        };
        let Ok(uri) = self.uri(&relative) else {
            return rename_fallback("source path is not a valid file URI");
        };
        match self
            .request(
                language,
                "textDocument/rename",
                json!({"textDocument":{"uri":uri.as_str()}, "position":position, "newName":new_name}),
            )
            .await
        {
            Ok(value) => RenameResult {
                backend: IntelligenceBackend::Lsp,
                edits: parse_workspace_edit(&self.uri_mapper, &value, self.config.max_results),
                note: None,
            },
            Err(_) => rename_fallback("language server unavailable; rename was not guessed"),
        }
    }

    async fn location_query(
        &self,
        path: &Path,
        position: Position,
        method: &'static str,
        fallback_role: SymbolRole,
    ) -> IntelligenceResult<Location> {
        let Some(language) = Language::for_path(path) else {
            return self.syntax_fallback(
                path,
                position,
                fallback_role,
                "no language server for this file type",
            );
        };
        let Ok(relative) = self.relative_path(path) else {
            return fallback(Vec::new(), "path is outside the workspace");
        };
        let Ok(uri) = self.uri(&relative) else {
            return fallback(Vec::new(), "source path is not a valid file URI");
        };
        match self.request(language, method, json!({"textDocument":{"uri":uri.as_str()}, "position":position, "context":{"includeDeclaration":true}})).await {
            Ok(value) => IntelligenceResult { backend: IntelligenceBackend::Lsp, items: parse_locations(&self.uri_mapper, value, self.config.max_results), note: None },
            Err(_) => self.syntax_fallback(&relative, position, fallback_role, "language server unavailable; used syntax index"),
        }
    }

    fn syntax_fallback(
        &self,
        path: &Path,
        position: Position,
        role: SymbolRole,
        note: &str,
    ) -> IntelligenceResult<Location> {
        let symbol_name = self
            .syntax
            .symbols_for_file(path)
            .ok()
            .and_then(|symbols| {
                symbols.into_iter().find(|symbol| {
                    let location = &symbol.location;
                    position.line as usize + 1 >= location.line
                        && (position.line as usize) < location.end_line
                        && position.character as usize + 1 >= location.column
                        && (position.character as usize) < location.end_column
                })
            })
            .map(|symbol| symbol.name);
        let items = symbol_name
            .and_then(|pattern| {
                self.syntax
                    .query(&SymbolQuery {
                        pattern,
                        roles: vec![role],
                        languages: Vec::new(),
                        limit: self.config.max_results,
                    })
                    .ok()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|symbol| source_location(symbol.location))
            .collect();
        fallback(items, note)
    }

    async fn request(
        &self,
        language: Language,
        method: &'static str,
        params: Value,
    ) -> Result<Value, LspError> {
        let slot = self.servers.get(&language).ok_or(LspError::Unavailable)?;
        let mut slot = slot.lock().await;
        for _ in 0..=self.config.max_restarts {
            if slot.client.is_none() {
                slot.client = Some(
                    start_client(
                        &self.root,
                        &slot.spec,
                        &self.config,
                        self.spawner.as_ref(),
                        &self.uri_mapper,
                    )
                    .await?,
                );
            }
            let result = {
                let client = slot.client.as_mut().ok_or(LspError::Unavailable)?;
                timeout(
                    self.config.request_timeout,
                    client.request(
                        &self.uri_mapper,
                        method,
                        params.clone(),
                        self.config.max_message_bytes,
                        self.config.max_diagnostics,
                        self.config.max_documents,
                    ),
                )
                .await
            };
            match result {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error @ LspError::Response(_))) => return Err(error),
                Ok(Err(error)) => {
                    stop_client(slot.client.take()).await;
                    if slot.restarts >= self.config.max_restarts {
                        return Err(error);
                    }
                }
                Err(_) => {
                    stop_client(slot.client.take()).await;
                    if slot.restarts >= self.config.max_restarts {
                        return Err(LspError::Timeout);
                    }
                }
            }
            slot.restarts = slot.restarts.saturating_add(1);
        }
        Err(LspError::Unavailable)
    }

    async fn update_document(
        &self,
        language: Language,
        uri: &str,
        cache_path: &Path,
        source: &str,
    ) -> Result<(), LspError> {
        let slot = self.servers.get(&language).ok_or(LspError::Unavailable)?;
        let mut slot = slot.lock().await;
        if slot.client.is_none() {
            slot.client = Some(
                start_client(
                    &self.root,
                    &slot.spec,
                    &self.config,
                    self.spawner.as_ref(),
                    &self.uri_mapper,
                )
                .await?,
            );
        }
        let client = slot.client.as_mut().ok_or(LspError::Unavailable)?;
        timeout(
            self.config.request_timeout,
            client.update_document(
                uri,
                cache_path,
                language,
                source,
                self.config.max_message_bytes,
                self.config.max_documents,
            ),
        )
        .await
        .map_err(|_| LspError::Timeout)??;
        Ok(())
    }

    async fn cached_diagnostics(&self, language: Language, path: &Path) -> Vec<Diagnostic> {
        let Some(slot) = self.servers.get(&language) else {
            return Vec::new();
        };
        let cache_path = self
            .uri_mapper
            .absolute_path(&self.root.join(path))
            .unwrap_or_else(|| path.to_path_buf());
        slot.lock()
            .await
            .client
            .as_ref()
            .and_then(|client| client.diagnostics.get(&cache_path).cloned())
            .unwrap_or_default()
    }

    async fn drain_notifications(&self, language: Language, expected_path: &Path) {
        let Some(slot) = self.servers.get(&language) else {
            return;
        };
        let mut slot = slot.lock().await;
        let Some(client) = slot.client.as_mut() else {
            return;
        };
        client
            .drain_notifications(
                &self.uri_mapper,
                self.config.max_message_bytes,
                self.config.max_diagnostics,
                self.config.max_documents,
                self.config
                    .notification_drain_timeout
                    .min(self.config.request_timeout),
                expected_path,
            )
            .await;
    }

    fn relative_path(&self, path: &Path) -> Result<PathBuf, LspError> {
        let absolute = if path.is_absolute() {
            std::fs::canonicalize(path).map_err(LspError::Io)?
        } else {
            self.root.join(path)
        };
        let relative = absolute
            .strip_prefix(&self.root)
            .map_err(|_| LspError::PathEscape)?;
        if relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            return Err(LspError::PathEscape);
        }
        Ok(relative.to_path_buf())
    }

    fn uri(&self, relative: &Path) -> Result<Url, LspError> {
        Url::from_file_path(self.root.join(relative)).map_err(|()| LspError::PathEscape)
    }
}

impl LspClient {
    async fn request(
        &mut self,
        uri_mapper: &WorkspaceUriMapper,
        method: &'static str,
        params: Value,
        max_bytes: usize,
        max_diagnostics: usize,
        max_documents: usize,
    ) -> Result<Value, LspError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        write_message(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
            max_bytes,
        )
        .await?;
        loop {
            let message = read_message(&mut self.stdout, max_bytes).await?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(LspError::Response(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("request failed")
                            .chars()
                            .take(512)
                            .collect(),
                    ));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                cache_published_diagnostics(
                    uri_mapper,
                    &mut self.diagnostics,
                    message.get("params"),
                    max_diagnostics,
                    max_documents,
                );
            } else if let Some(server_request_id) = message.get("id").cloned()
                && message.get("method").and_then(Value::as_str).is_some()
            {
                // This intentionally small client does not implement
                // server-to-client requests. Reply instead of ignoring them so
                // a server never waits forever for an unsupported capability.
                write_message(
                    &mut self.stdin,
                    &json!({"jsonrpc":"2.0", "id":server_request_id, "error":{"code":-32601, "message":"client method not supported"}}),
                    max_bytes,
                )
                .await?;
            }
        }
    }

    async fn notify(
        &mut self,
        method: &'static str,
        params: Value,
        max_bytes: usize,
    ) -> Result<(), LspError> {
        write_message(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0", "method":method, "params":params}),
            max_bytes,
        )
        .await
    }

    async fn drain_notifications(
        &mut self,
        uri_mapper: &WorkspaceUriMapper,
        max_bytes: usize,
        max_diagnostics: usize,
        max_documents: usize,
        duration: Duration,
        expected_path: &Path,
    ) {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(Ok(message)) =
                timeout(remaining, read_message(&mut self.stdout, max_bytes)).await
            else {
                break;
            };
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                cache_published_diagnostics(
                    uri_mapper,
                    &mut self.diagnostics,
                    message.get("params"),
                    max_diagnostics,
                    max_documents,
                );
                if self.diagnostics.contains_key(expected_path) {
                    break;
                }
            } else if let Some(server_request_id) = message.get("id").cloned()
                && message.get("method").and_then(Value::as_str).is_some()
            {
                let _ = write_message(
                    &mut self.stdin,
                    &json!({"jsonrpc":"2.0", "id":server_request_id, "error":{"code":-32601, "message":"client method not supported"}}),
                    max_bytes,
                )
                .await;
            }
        }
    }

    async fn update_document(
        &mut self,
        uri: &str,
        cache_path: &Path,
        language: Language,
        source: &str,
        max_bytes: usize,
        max_documents: usize,
    ) -> Result<(), LspError> {
        self.diagnostics.remove(cache_path);
        if let Some(version) = self.document_versions.get_mut(uri) {
            *version = version.saturating_add(1);
            let version = *version;
            self.notify(
                "textDocument/didChange",
                json!({"textDocument":{"uri":uri, "version":version}, "contentChanges":[{"text":source}]}),
                max_bytes,
            )
            .await
        } else {
            let uri = uri.to_owned();
            bound_map_for_insert(&mut self.document_versions, &uri, max_documents);
            self.document_versions.insert(uri.clone(), 1);
            self.notify(
                "textDocument/didOpen",
                json!({"textDocument":{"uri":uri, "languageId":language_id(language), "version":1, "text":source}}),
                max_bytes,
            )
            .await
        }
    }
}

async fn start_client(
    root: &Path,
    spec: &LspServerConfig,
    config: &LspConfig,
    spawner: &dyn LspProcessSpawner,
    uri_mapper: &WorkspaceUriMapper,
) -> Result<LspClient, LspError> {
    let child = spawner.spawn(root, spec).await?;
    let mut client = LspClient {
        child: child.handle,
        stdin: child.stdin,
        stdout: BufReader::new(child.stdout),
        next_id: 1,
        diagnostics: BTreeMap::new(),
        document_versions: BTreeMap::new(),
    };
    let root_uri = Url::from_directory_path(root).map_err(|()| LspError::PathEscape)?;
    timeout(config.request_timeout, client.request(uri_mapper, "initialize", json!({"processId":std::process::id(), "rootUri":root_uri.as_str(), "capabilities":{"textDocument":{"publishDiagnostics":{}, "definition":{}, "references":{}, "rename":{}}}, "workspaceFolders":[{"uri":root_uri.as_str(),"name":root.file_name().and_then(|v|v.to_str()).unwrap_or("workspace")}] }), config.max_message_bytes, config.max_diagnostics, config.max_documents)).await.map_err(|_| LspError::Timeout)??;
    client
        .notify("initialized", json!({}), config.max_message_bytes)
        .await?;
    Ok(client)
}

async fn stop_client(client: Option<LspClient>) {
    if let Some(mut client) = client {
        let _ = client.child.kill().await;
    }
}

async fn write_message(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &Value,
    max_bytes: usize,
) -> Result<(), LspError> {
    let body = serde_json::to_vec(value)
        .map_err(|_| LspError::Protocol("could not serialize JSON-RPC message"))?;
    if body.len() > max_bytes {
        return Err(LspError::Protocol("outbound message exceeds size limit"));
    }
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_message(
    reader: &mut (impl AsyncBufRead + Unpin),
    max_bytes: usize,
) -> Result<Value, LspError> {
    let mut total = 0usize;
    let mut content_length = None;
    loop {
        let line = read_header_line(reader, MAX_HEADER_BYTES.saturating_sub(total)).await?;
        total = total.saturating_add(line.len());
        if line == b"\r\n" {
            break;
        }
        let line = line
            .strip_suffix(b"\r\n")
            .ok_or(LspError::Protocol("headers must use CRLF framing"))?;
        let line = std::str::from_utf8(line)
            .map_err(|_| LspError::Protocol("header is not valid ASCII"))?;
        let (name, value) = line
            .split_once(':')
            .ok_or(LspError::Protocol("malformed header"))?;
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(LspError::Protocol("duplicate Content-Length"));
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| LspError::Protocol("invalid Content-Length"))?,
            );
        }
    }
    let length = content_length.ok_or(LspError::Protocol("missing Content-Length"))?;
    if length > max_bytes {
        return Err(LspError::Protocol("message exceeds size limit"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|_| LspError::Protocol("invalid JSON-RPC payload"))
}

async fn read_header_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    remaining: usize,
) -> Result<Vec<u8>, LspError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(LspError::Protocol("unexpected EOF in header"));
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position.saturating_add(1));
        if line.len().saturating_add(consumed) > remaining {
            return Err(LspError::Protocol("header exceeds size limit"));
        }
        let complete = available.get(consumed.saturating_sub(1)) == Some(&b'\n');
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if complete {
            return Ok(line);
        }
    }
}

fn language_id(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::Python => "python",
        Language::TypeScript => "typescript",
    }
}

fn fallback<T>(items: Vec<T>, note: &str) -> IntelligenceResult<T> {
    IntelligenceResult {
        backend: IntelligenceBackend::TreeSitter,
        items,
        note: Some(note.to_owned()),
    }
}
fn rename_fallback(note: &str) -> RenameResult {
    RenameResult {
        backend: IntelligenceBackend::TreeSitter,
        edits: Vec::new(),
        note: Some(note.to_owned()),
    }
}

fn source_location(location: SourceLocation) -> Location {
    Location {
        path: location.path,
        range: Range {
            start: Position {
                line: saturating_u32(location.line.saturating_sub(1)),
                character: saturating_u32(location.column.saturating_sub(1)),
            },
            end: Position {
                line: saturating_u32(location.end_line.saturating_sub(1)),
                character: saturating_u32(location.end_column.saturating_sub(1)),
            },
        },
    }
}

fn parse_locations(uri_mapper: &WorkspaceUriMapper, value: Value, limit: usize) -> Vec<Location> {
    let values = match value {
        Value::Array(values) => values,
        Value::Null => Vec::new(),
        value => vec![value],
    };
    values
        .into_iter()
        .filter_map(|value| {
            let uri = value
                .get("uri")
                .or_else(|| value.get("targetUri"))?
                .as_str()?;
            let range = value
                .get("range")
                .or_else(|| value.get("targetSelectionRange"))?
                .clone();
            Some(Location {
                path: uri_mapper.uri_path(uri)?,
                range: serde_json::from_value(range).ok()?,
            })
        })
        .take(limit)
        .collect()
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn parse_workspace_edit(
    uri_mapper: &WorkspaceUriMapper,
    value: &Value,
    limit: usize,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    if let Some(changes) = value.get("changes").and_then(Value::as_object) {
        for (uri, values) in changes {
            let Some(path) = uri_mapper.uri_path(uri) else {
                continue;
            };
            for value in values.as_array().into_iter().flatten() {
                if edits.len() >= limit {
                    return edits;
                }
                if let (Some(range), Some(new_text)) = (
                    value
                        .get("range")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                    value.get("newText").and_then(Value::as_str),
                ) {
                    edits.push(TextEdit {
                        path: path.clone(),
                        range,
                        new_text: new_text.chars().take(64 * 1024).collect(),
                    });
                }
            }
        }
    }
    if let Some(document_changes) = value.get("documentChanges").and_then(Value::as_array) {
        for change in document_changes {
            let Some(uri) = change
                .get("textDocument")
                .and_then(|document| document.get("uri"))
                .and_then(Value::as_str)
            else {
                // File create/rename/delete resource operations are not safe to
                // infer or apply through the rename-query tier.
                continue;
            };
            let Some(path) = uri_mapper.uri_path(uri) else {
                continue;
            };
            for value in change
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if edits.len() >= limit {
                    return edits;
                }
                if let (Some(range), Some(new_text)) = (
                    value
                        .get("range")
                        .and_then(|value| serde_json::from_value(value.clone()).ok()),
                    value.get("newText").and_then(Value::as_str),
                ) {
                    edits.push(TextEdit {
                        path: path.clone(),
                        range,
                        new_text: new_text.chars().take(64 * 1024).collect(),
                    });
                }
            }
        }
    }
    edits
}

fn parse_pull_diagnostics(path: &Path, value: &Value) -> Vec<Diagnostic> {
    value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| parse_diagnostic(path.to_path_buf(), value))
        .collect()
}

fn parse_diagnostic(path: PathBuf, value: &Value) -> Option<Diagnostic> {
    let severity = match value.get("severity").and_then(Value::as_u64) {
        Some(1) => DiagnosticSeverity::Error,
        Some(2) => DiagnosticSeverity::Warning,
        Some(3) => DiagnosticSeverity::Information,
        Some(4) => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Unknown,
    };
    Some(Diagnostic {
        path,
        range: serde_json::from_value(value.get("range")?.clone()).ok()?,
        severity,
        message: value
            .get("message")?
            .as_str()?
            .chars()
            .take(8 * 1024)
            .collect(),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(|v| v.chars().take(128).collect()),
        code: value
            .get("code")
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .map(|v| v.chars().take(128).collect()),
    })
}

fn cache_published_diagnostics(
    uri_mapper: &WorkspaceUriMapper,
    cache: &mut BTreeMap<PathBuf, Vec<Diagnostic>>,
    params: Option<&Value>,
    limit: usize,
    document_limit: usize,
) {
    let Some(params) = params else {
        return;
    };
    let Some(path) = params
        .get("uri")
        .and_then(Value::as_str)
        .and_then(|uri| uri_mapper.uri_path(uri))
    else {
        return;
    };
    let diagnostics = params
        .get("diagnostics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| parse_diagnostic(path.clone(), value))
        .take(limit)
        .collect();
    bound_map_for_insert(cache, &path, document_limit);
    cache.insert(path, diagnostics);
}

fn bound_map_for_insert<K: Clone + Ord, V>(map: &mut BTreeMap<K, V>, key: &K, limit: usize) {
    let limit = limit.max(1);
    if !map.contains_key(key)
        && map.len() >= limit
        && let Some(oldest) = map.keys().next().cloned()
    {
        map.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use tempfile::tempdir;
    use tokio::io::duplex;

    struct UnavailableSpawner;

    #[async_trait]
    impl LspProcessSpawner for UnavailableSpawner {
        async fn spawn(
            &self,
            _workspace: &Path,
            _server: &LspServerConfig,
        ) -> Result<SpawnedLspProcess, LspError> {
            Err(LspError::Unavailable)
        }
    }

    struct NoopHandle;

    #[async_trait]
    impl LspProcessHandle for NoopHandle {
        async fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct PushDiagnosticsSpawner;

    #[async_trait]
    impl LspProcessSpawner for PushDiagnosticsSpawner {
        async fn spawn(
            &self,
            _workspace: &Path,
            _server: &LspServerConfig,
        ) -> Result<SpawnedLspProcess, LspError> {
            let (client_stdin, server_stdin) = duplex(64 * 1024);
            let (server_stdout, client_stdout) = duplex(64 * 1024);
            tokio::spawn(fake_push_diagnostics_server(server_stdin, server_stdout));
            Ok(SpawnedLspProcess {
                handle: Box::new(NoopHandle),
                stdin: Box::pin(client_stdin),
                stdout: Box::pin(client_stdout),
            })
        }
    }

    struct CrossRootSpawner {
        target_uri: String,
    }

    #[async_trait]
    impl LspProcessSpawner for CrossRootSpawner {
        async fn spawn(
            &self,
            _workspace: &Path,
            _server: &LspServerConfig,
        ) -> Result<SpawnedLspProcess, LspError> {
            let (client_stdin, server_stdin) = duplex(64 * 1024);
            let (server_stdout, client_stdout) = duplex(64 * 1024);
            tokio::spawn(fake_cross_root_server(
                server_stdin,
                server_stdout,
                self.target_uri.clone(),
            ));
            Ok(SpawnedLspProcess {
                handle: Box::new(NoopHandle),
                stdin: Box::pin(client_stdin),
                stdout: Box::pin(client_stdout),
            })
        }
    }

    async fn fake_cross_root_server(
        stdin: tokio::io::DuplexStream,
        mut stdout: tokio::io::DuplexStream,
        target_uri: String,
    ) {
        let mut stdin = BufReader::new(stdin);
        let initialize = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
            .await
            .expect("initialize");
        write_message(
            &mut stdout,
            &json!({"jsonrpc":"2.0", "id":initialize["id"], "result":{"capabilities":{}}}),
            DEFAULT_MAX_MESSAGE_BYTES,
        )
        .await
        .expect("initialize response");
        let _initialized = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
            .await
            .expect("initialized");
        for _ in 0..3 {
            let request = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
                .await
                .expect("request");
            let result = match request["method"].as_str() {
                Some("textDocument/definition") => json!({
                    "uri":target_uri,
                    "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}
                }),
                Some("textDocument/references") => json!([{
                    "uri":target_uri,
                    "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}
                }]),
                Some("textDocument/rename") => json!({"changes":{
                    target_uri.clone(): [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"Cat"}]
                }}),
                _ => Value::Null,
            };
            write_message(
                &mut stdout,
                &json!({"jsonrpc":"2.0", "id":request["id"], "result":result}),
                DEFAULT_MAX_MESSAGE_BYTES,
            )
            .await
            .expect("response");
        }
    }

    async fn fake_push_diagnostics_server(
        stdin: tokio::io::DuplexStream,
        mut stdout: tokio::io::DuplexStream,
    ) {
        let mut stdin = BufReader::new(stdin);
        let initialize = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
            .await
            .expect("initialize");
        write_message(
            &mut stdout,
            &json!({"jsonrpc":"2.0", "id":initialize["id"], "result":{"capabilities":{}}}),
            DEFAULT_MAX_MESSAGE_BYTES,
        )
        .await
        .expect("initialize response");
        let _initialized = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
            .await
            .expect("initialized");
        for revision in 0..2 {
            let update = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
                .await
                .expect("document update");
            let uri = update["params"]["textDocument"]["uri"]
                .as_str()
                .expect("document URI")
                .to_owned();
            let pull = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
                .await
                .expect("diagnostic pull");
            write_message(
                &mut stdout,
                &json!({"jsonrpc":"2.0", "id":pull["id"], "error":{"code":-32601,"message":"pull unsupported"}}),
                DEFAULT_MAX_MESSAGE_BYTES,
            )
            .await
            .expect("pull rejection");
            if revision == 1 {
                tokio::time::sleep(Duration::from_millis(350)).await;
                write_message(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0", "method":"textDocument/publishDiagnostics", "params":{"uri":uri.replace("lib.rs", "other.rs"),"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"severity":2,"message":"unrelated diagnostic"}]}}),
                    DEFAULT_MAX_MESSAGE_BYTES,
                )
                .await
                .expect("publish unrelated diagnostics");
                tokio::time::sleep(Duration::from_millis(350)).await;
                write_message(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0", "method":"textDocument/publishDiagnostics", "params":{"uri":uri,"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"severity":1,"message":"push-only diagnostic"}]}}),
                    DEFAULT_MAX_MESSAGE_BYTES,
                )
                .await
                .expect("publish diagnostics");
            }
        }
    }

    #[tokio::test]
    async fn framing_accepts_unknown_headers_and_exact_body() {
        let (mut write, read) = duplex(1024);
        let mut read = BufReader::new(read);
        write.write_all(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 24\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1}").await.expect("write");
        let value = read_message(&mut read, 1024).await.expect("message");
        assert_eq!(value["id"], 1);
    }

    #[test]
    fn document_version_and_diagnostic_caches_stay_bounded() {
        let mut versions = BTreeMap::new();
        for key in ["c", "b", "a"] {
            let key = key.to_owned();
            bound_map_for_insert(&mut versions, &key, 2);
            versions.insert(key, 1);
        }
        assert_eq!(versions.len(), 2);

        let mut diagnostics = BTreeMap::new();
        for key in ["c.rs", "b.rs", "a.rs"] {
            let key = PathBuf::from(key);
            bound_map_for_insert(&mut diagnostics, &key, 2);
            diagnostics.insert(key, Vec::<Diagnostic>::new());
        }
        assert_eq!(diagnostics.len(), 2);
    }

    #[test]
    fn intelligence_layer_has_no_ambient_process_spawn_path() {
        let source = include_str!("lsp.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(!production.contains("tokio::process::Command"));
        assert!(!production.contains("std::process::Command"));
        assert!(!production.contains(".spawn()?"));
    }

    #[tokio::test]
    async fn framing_rejects_duplicate_or_oversized_lengths() {
        for bytes in [
            b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
            b"Content-Length: 999\r\n\r\n".as_slice(),
        ] {
            let (mut write, read) = duplex(1024);
            let mut read = BufReader::new(read);
            write.write_all(bytes).await.expect("write");
            assert!(read_message(&mut read, 16).await.is_err());
        }
    }

    #[tokio::test]
    async fn framing_rejects_unterminated_headers_at_the_cap() {
        let (mut write, read) = duplex(MAX_HEADER_BYTES * 2);
        let mut read = BufReader::new(read);
        write
            .write_all(&vec![b'x'; MAX_HEADER_BYTES + 1])
            .await
            .expect("write");
        assert!(matches!(
            read_message(&mut read, 16).await,
            Err(LspError::Protocol("header exceeds size limit"))
        ));
    }

    #[tokio::test]
    async fn absent_server_degrades_definition_to_syntax_index() {
        let root = tempdir().expect("root");
        let syntax = Arc::new(SymbolIndex::new(root.path()).expect("index"));
        syntax
            .update_source("lib.rs", "pub struct Dog;\nfn f(_: Dog) {}\n")
            .expect("source");
        let intel = CodeIntelligence::new(
            root.path(),
            syntax,
            LspConfig {
                servers: Vec::new(),
                ..LspConfig::default()
            },
            Arc::new(UnavailableSpawner),
        )
        .expect("intel");
        let result = intel
            .definition(
                "lib.rs",
                Position {
                    line: 1,
                    character: 8,
                },
            )
            .await;
        assert_eq!(result.backend, IntelligenceBackend::TreeSitter);
        assert!(!result.items.is_empty());
    }

    #[tokio::test]
    async fn push_only_diagnostics_arrive_in_same_turn_after_did_change() {
        let root = tempdir().expect("root");
        std::fs::write(root.path().join("lib.rs"), "fn first() {}\n").expect("source");
        std::fs::write(root.path().join("other.rs"), "fn other() {}\n").expect("other source");
        let syntax = Arc::new(SymbolIndex::new(root.path()).expect("index"));
        let intel = CodeIntelligence::new(
            root.path(),
            syntax,
            LspConfig {
                servers: vec![LspServerConfig {
                    language: Language::Rust,
                    command: PathBuf::from("fake-rust-analyzer"),
                    args: Vec::new(),
                }],
                request_timeout: Duration::from_secs(1),
                notification_drain_timeout: Duration::from_secs(3),
                ..LspConfig::default()
            },
            Arc::new(PushDiagnosticsSpawner),
        )
        .expect("intel");
        let first = intel.diagnostics("lib.rs", "fn first() {}\n").await;
        assert!(first.items.is_empty());
        let changed = intel.diagnostics("lib.rs", "fn changed() {}\n").await;
        assert_eq!(changed.backend, IntelligenceBackend::Lsp);
        assert_eq!(changed.items.len(), 1);
        assert_eq!(changed.items[0].message, "push-only diagnostic");
    }

    #[tokio::test]
    async fn two_root_server_results_are_retained_and_virtualized() {
        let first = tempdir().expect("first root");
        let second = tempdir().expect("second root");
        std::fs::write(first.path().join("lib.rs"), "fn use_dog() {}\n").expect("first source");
        std::fs::write(second.path().join("other.rs"), "struct Dog;\n").expect("second source");
        let roots = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        let mapper = Arc::new(WorkspaceUriMapper::new(&roots).expect("URI mapper"));
        let target_uri = Url::from_file_path(second.path().join("other.rs"))
            .expect("target URI")
            .to_string();
        let intel = CodeIntelligence::new_with_uri_mapper(
            first.path(),
            Arc::new(SymbolIndex::new(first.path()).expect("index")),
            LspConfig {
                servers: vec![LspServerConfig {
                    language: Language::Rust,
                    command: PathBuf::from("fake-rust-analyzer"),
                    args: Vec::new(),
                }],
                ..LspConfig::default()
            },
            Arc::new(CrossRootSpawner { target_uri }),
            mapper,
        )
        .expect("intel");
        let position = Position {
            line: 0,
            character: 3,
        };
        let definition = intel.definition("lib.rs", position).await;
        let references = intel.references("lib.rs", position).await;
        let rename = intel.rename("lib.rs", position, "Cat").await;
        let expected = Path::new("@root/1/other.rs");
        assert_eq!(definition.items[0].path, expected);
        assert_eq!(references.items[0].path, expected);
        assert_eq!(rename.edits[0].path, expected);
    }

    #[test]
    fn workspace_edits_drop_outside_uris_and_bound_text() {
        let root = tempdir().expect("root");
        std::fs::write(root.path().join("lib.rs"), "Dog").expect("source");
        let mapper = WorkspaceUriMapper::new(&[root.path().to_path_buf()]).expect("URI mapper");
        let inside = Url::from_file_path(root.path().join("lib.rs")).expect("uri");
        let edits = parse_workspace_edit(
            &mapper,
            &json!({"changes": {inside.as_str(): [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"Dog"}], "file:///tmp/outside.rs":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"Bad"}]}}),
            10,
        );
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, PathBuf::from("lib.rs"));
    }
}
