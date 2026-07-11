//! CLI composition for the headless multi-session engine host.

use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
#[cfg(unix)]
use std::{
    io::Read as _,
    os::{fd::OwnedFd, unix::ffi::OsStrExt as _},
    time::Instant,
};

use async_trait::async_trait;
use rw_core::runtime_support::PricingTable;
use rw_core::{
    AttachmentData, CommandDescriptor, Config, CreateSessionRequest, HostError, HostQueryService,
    HostedSession, ModelAlias, ModelCacheBehavior, ModelCapabilities, ModelDescriptor,
    SessionDescriptor, SessionFactory, SessionId, WorkspaceFileMatch, WorkspaceFilePreview,
    WorkspaceStatus, builtin_command_registry,
};
use rw_store::config::ConfigLoader;

use crate::{
    PermissionMode,
    runtime::{
        HostedProviderMode, HostedSessionComposition, compose_hosted_actor,
        load_session_metadata_any, load_session_workspace_roots, new_session_id,
    },
};

const MAX_SEARCH_RESULTS: usize = 1_000;
const MAX_SESSION_RESULTS: usize = 10_000;
#[cfg(unix)]
const MAX_SEARCH_ENTRIES: usize = 50_000;
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
const QUERY_DEADLINE: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub(crate) struct CliHostOptions {
    pub storage_root: PathBuf,
    pub credentials_path: PathBuf,
    pub config: Config,
    pub allowed_workspaces: Vec<PathBuf>,
    pub permission_mode: Option<PermissionMode>,
    pub max_turns: usize,
    pub provider_mode: HostedProviderMode,
}

impl CliHostOptions {
    pub(crate) fn from_environment(
        allowed_workspaces: Vec<PathBuf>,
        dangerously_trust: bool,
        permission_mode: Option<PermissionMode>,
        max_turns: usize,
        provider_mode: HostedProviderMode,
    ) -> Result<Self, HostError> {
        let loader = ConfigLoader::from_environment()
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        let credentials_path = loader.credentials_path().clone();
        let storage_root = credentials_path
            .parent()
            .ok_or_else(|| HostError::Persistence("configuration root is unavailable".to_owned()))?
            .to_path_buf();
        let loader = if dangerously_trust {
            loader.dangerously_trust_project()
        } else {
            loader
        };
        let config = loader
            .load()
            .map_err(|error| HostError::Persistence(error.to_string()))?
            .config;
        Ok(Self {
            storage_root,
            credentials_path,
            config,
            allowed_workspaces,
            permission_mode,
            max_turns,
            provider_mode,
        })
    }
}

#[derive(Clone)]
pub(crate) struct CliSessionFactory {
    options: Arc<CliHostOptions>,
    allowed_workspaces: Arc<Vec<PathBuf>>,
}

impl CliSessionFactory {
    pub(crate) fn new(mut options: CliHostOptions) -> Result<Self, HostError> {
        if options.max_turns == 0 || options.allowed_workspaces.is_empty() {
            return Err(HostError::Protocol(
                "host requires a turn limit and at least one authorized workspace".to_owned(),
            ));
        }
        let mut allowed = options
            .allowed_workspaces
            .iter()
            .map(|root| {
                fs::canonicalize(root)
                    .map_err(|_| HostError::Protocol("authorized workspace is invalid".to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        allowed.sort();
        allowed.dedup();
        options.allowed_workspaces.clone_from(&allowed);
        fs::create_dir_all(&options.storage_root)
            .map_err(|_| HostError::Persistence("host storage could not initialize".to_owned()))?;
        Ok(Self {
            options: Arc::new(options),
            allowed_workspaces: Arc::new(allowed),
        })
    }

    fn authorize_workspace(&self, requested: &str) -> Result<PathBuf, HostError> {
        let requested = Path::new(requested);
        if !requested.is_absolute() {
            return Err(HostError::Protocol(
                "workspace must be an absolute path on the engine host".to_owned(),
            ));
        }
        let canonical = fs::canonicalize(requested)
            .map_err(|_| HostError::Protocol("workspace is unavailable".to_owned()))?;
        if !self
            .allowed_workspaces
            .iter()
            .any(|root| canonical == *root || canonical.starts_with(root))
        {
            return Err(HostError::Protocol(
                "workspace is outside the authorized roots".to_owned(),
            ));
        }
        Ok(canonical)
    }

    fn workspace_for_session(&self, descriptor: &SessionDescriptor) -> Result<PathBuf, HostError> {
        let metadata =
            load_session_metadata_any(&self.options.storage_root, &descriptor.session_id.0)
                .map_err(|_| {
                    HostError::Query("session workspace metadata is unavailable".to_owned())
                })?;
        let workspace = self.authorize_workspace_path(&metadata.workspace)?;
        if workspace_name(&workspace) != descriptor.workspace_name {
            return Err(HostError::Query(
                "session workspace descriptor does not match durable metadata".to_owned(),
            ));
        }
        Ok(workspace)
    }

    fn workspace_roots_for_session(
        &self,
        descriptor: &SessionDescriptor,
    ) -> Result<Vec<PathBuf>, HostError> {
        let primary = self.workspace_for_session(descriptor)?;
        let configured = load_session_workspace_roots(
            &self.options.storage_root,
            &primary,
            &descriptor.session_id.0,
        )
        .map_err(|_| HostError::Query("session workspace roots are unavailable".to_owned()))?;
        let mut roots = Vec::with_capacity(configured.len());
        for (index, root) in configured.into_iter().enumerate() {
            let canonical = fs::canonicalize(&root).map_err(|_| {
                HostError::Query("session workspace root is unavailable".to_owned())
            })?;
            if index == 0 && canonical != primary {
                return Err(HostError::Query(
                    "session primary workspace root changed".to_owned(),
                ));
            }
            roots.push(canonical);
        }
        Ok(roots)
    }

    fn authorize_workspace_path(&self, workspace: &Path) -> Result<PathBuf, HostError> {
        let canonical = fs::canonicalize(workspace)
            .map_err(|_| HostError::Query("session workspace is unavailable".to_owned()))?;
        if self
            .allowed_workspaces
            .iter()
            .any(|root| canonical == *root || canonical.starts_with(root))
        {
            Ok(canonical)
        } else {
            Err(HostError::Query(
                "session workspace is outside authorized roots".to_owned(),
            ))
        }
    }

    async fn compose(
        &self,
        session_id: SessionId,
        workspace: PathBuf,
        model: Option<ModelAlias>,
        resume: bool,
    ) -> Result<HostedSession, HostError> {
        let runtime = compose_hosted_actor(HostedSessionComposition {
            workspace: workspace.clone(),
            additional_workspaces: self
                .allowed_workspaces
                .iter()
                .filter(|root| **root != workspace)
                .cloned()
                .collect(),
            storage_root: self.options.storage_root.clone(),
            credentials_path: self.options.credentials_path.clone(),
            config: self.options.config.clone(),
            session_id: session_id.clone(),
            requested_model: model.map(|model| model.0),
            resume,
            permission_mode: self.options.permission_mode,
            max_turns: self.options.max_turns,
            provider_mode: self.options.provider_mode.clone(),
        })
        .await
        .map_err(|error| {
            tracing::error!(session_id = %session_id.0, reason = %error, "hosted session composition failed");
            HostError::Persistence("session runtime could not be composed".to_owned())
        })?;
        Ok(HostedSession::new(
            SessionDescriptor {
                session_id,
                workspace_name: workspace_name(&workspace),
                model: ModelAlias(runtime.model_alias),
                driver_client_id: runtime.driver_client_id,
                shell_active: runtime.shell_active,
            },
            runtime.handle,
        ))
    }

    fn persisted_descriptor(&self, session_id: &str) -> Result<SessionDescriptor, HostError> {
        let metadata = load_session_metadata_any(&self.options.storage_root, session_id)
            .map_err(|_| HostError::Persistence("session metadata is unavailable".to_owned()))?;
        let workspace = self.authorize_workspace_path(&metadata.workspace)?;
        Ok(SessionDescriptor {
            session_id: SessionId(session_id.to_owned()),
            workspace_name: workspace_name(&workspace),
            model: ModelAlias(metadata.model_alias),
            // Persisted sessions are inactive until resumed. Live descriptors
            // from the host registry replace these entries after opening.
            driver_client_id: None,
            shell_active: false,
        })
    }

    fn persisted_sessions_blocking(&self) -> Result<Vec<SessionDescriptor>, HostError> {
        let sessions = self.options.storage_root.join("sessions");
        let entries = match fs::read_dir(sessions) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => {
                return Err(HostError::Persistence(
                    "session directory could not be listed".to_owned(),
                ));
            }
        };
        let mut descriptors = Vec::new();
        for entry in entries.take(MAX_SESSION_RESULTS).flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Ok(descriptor) = self.persisted_descriptor(&session_id) {
                descriptors.push(descriptor);
            }
        }
        descriptors.sort_by(|left, right| left.session_id.0.cmp(&right.session_id.0));
        Ok(descriptors)
    }
}

#[async_trait]
impl SessionFactory for CliSessionFactory {
    fn allocate_session_id(&self) -> Result<SessionId, HostError> {
        new_session_id()
            .map(SessionId)
            .map_err(|_| HostError::Persistence("session id allocation failed".to_owned()))
    }

    async fn create(&self, request: CreateSessionRequest) -> Result<HostedSession, HostError> {
        let workspace = self.authorize_workspace(&request.workspace)?;
        self.compose(request.session_id, workspace, request.model, false)
            .await
    }

    async fn resume(&self, session_id: &SessionId) -> Result<HostedSession, HostError> {
        let metadata = load_session_metadata_any(&self.options.storage_root, &session_id.0)
            .map_err(|_| HostError::Persistence("session metadata is unavailable".to_owned()))?;
        let workspace = self.authorize_workspace_path(&metadata.workspace)?;
        self.compose(session_id.clone(), workspace, None, true)
            .await
    }

    async fn persisted_sessions(&self) -> Result<Vec<SessionDescriptor>, HostError> {
        let factory = self.clone();
        tokio::time::timeout(
            QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || factory.persisted_sessions_blocking()),
        )
        .await
        .map_err(|_| HostError::Query("session listing deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("session listing worker failed".to_owned()))?
    }
}

#[async_trait]
impl HostQueryService for CliSessionFactory {
    async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError> {
        let registry = builtin_command_registry().map_err(HostError::from)?;
        Ok(registry
            .descriptors()
            .map(|descriptor| CommandDescriptor {
                name: descriptor.name().to_owned(),
                description: descriptor.description().to_owned(),
                usage: descriptor.argument_hint().unwrap_or_default().to_owned(),
            })
            .collect())
    }

    async fn model_descriptors(&self) -> Result<Vec<ModelDescriptor>, HostError> {
        let pricing = PricingTable::bundled().ok();
        let mut descriptors = BTreeMap::new();
        for (alias, candidates) in &self.options.config.models.aliases {
            let capabilities =
                conservative_alias_capabilities(candidates, &self.options.config, pricing.as_ref());
            descriptors.insert(
                alias.clone(),
                ModelDescriptor {
                    alias: ModelAlias(alias.clone()),
                    capabilities,
                },
            );
        }
        descriptors
            .entry(self.options.config.models.default.clone())
            .or_insert_with(|| ModelDescriptor {
                alias: ModelAlias(self.options.config.models.default.clone()),
                capabilities: unknown_capabilities(),
            });
        Ok(descriptors.into_values().collect())
    }

    async fn search_workspace_files(
        &self,
        session: &SessionDescriptor,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
        let workspaces = self.workspace_roots_for_session(session)?;
        let query = query.to_owned();
        let limit = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .clamp(1, MAX_SEARCH_RESULTS);
        tokio::time::timeout(
            QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || search_workspaces(&workspaces, &query, limit)),
        )
        .await
        .map_err(|_| HostError::Query("workspace search deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace search worker failed".to_owned()))?
    }

    async fn preview_workspace_file(
        &self,
        session: &SessionDescriptor,
        path: &str,
        max_bytes: u32,
    ) -> Result<WorkspaceFilePreview, HostError> {
        let workspaces = self.workspace_roots_for_session(session)?;
        let (root_index, relative) = split_virtual_path(path)?;
        let workspace = workspaces
            .get(root_index)
            .cloned()
            .ok_or_else(|| HostError::Query("workspace root index is not authorized".to_owned()))?;
        let rendered_path = path.to_owned();
        let maximum = usize::try_from(max_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_PREVIEW_BYTES);
        if maximum == 0 {
            return Err(HostError::Query(
                "preview byte limit must not be zero".to_owned(),
            ));
        }
        tokio::time::timeout(
            QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || {
                let mut preview = preview_file(&workspace, &relative, maximum)?;
                preview.path = rendered_path;
                Ok(preview)
            }),
        )
        .await
        .map_err(|_| HostError::Query("workspace preview deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace preview worker failed".to_owned()))?
    }

    async fn workspace_status(
        &self,
        session: &SessionDescriptor,
    ) -> Result<WorkspaceStatus, HostError> {
        let workspace = self.workspace_for_session(session)?;
        let name = session.workspace_name.clone();
        tokio::time::timeout(
            QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || read_workspace_status(&workspace, name)),
        )
        .await
        .map_err(|_| HostError::Query("workspace status deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace status worker failed".to_owned()))?
    }
}

fn workspace_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .to_owned()
}

fn unknown_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        tool_calling: false,
        vision: false,
        thinking: false,
        cache_behavior: ModelCacheBehavior::None,
    }
}

fn conservative_alias_capabilities(
    candidates: &[String],
    config: &Config,
    pricing: Option<&PricingTable>,
) -> ModelCapabilities {
    if candidates.is_empty() {
        return unknown_capabilities();
    }
    let mut tool_calling = true;
    let mut thinking = true;
    let mut cache_behavior = None;
    for candidate in candidates {
        let Some((provider_name, model)) = candidate.split_once('/') else {
            return unknown_capabilities();
        };
        let Some(provider) = config.providers.get(provider_name) else {
            return unknown_capabilities();
        };
        let catalog_provider = match provider.kind.as_str() {
            "anthropic" => "anthropic",
            "openai"
            | "openai_responses"
            | "openai_chat"
            | "openai_codex"
            | "openai_subscription" => "openai",
            // Compatible and dynamically discovered providers are unknown
            // until their own metadata has been fetched.
            _ => return unknown_capabilities(),
        };
        let key = format!("{catalog_provider}/{model}");
        let Some(model) = pricing.and_then(|table| table.models.get(&key)) else {
            return unknown_capabilities();
        };
        tool_calling &= model.supports_tools;
        thinking &= model.supports_thinking && !model.reasoning_efforts.is_empty();
        let candidate_cache = match provider.kind.as_str() {
            "anthropic" => ModelCacheBehavior::Explicit,
            "openai"
            | "openai_responses"
            | "openai_chat"
            | "openai_codex"
            | "openai_subscription" => ModelCacheBehavior::ProviderManaged,
            _ => ModelCacheBehavior::None,
        };
        cache_behavior = match cache_behavior {
            None => Some(candidate_cache),
            Some(existing) if existing == candidate_cache => Some(existing),
            Some(_) => Some(ModelCacheBehavior::None),
        };
    }
    ModelCapabilities {
        tool_calling,
        // The current pricing catalog has no authoritative vision field.
        vision: false,
        thinking,
        cache_behavior: cache_behavior.unwrap_or(ModelCacheBehavior::None),
    }
}

fn safe_relative_path(value: &str) -> Result<PathBuf, HostError> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err(HostError::Query(
            "workspace path must be a non-empty normalized relative path".to_owned(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(HostError::Query(
                "workspace path must be a non-empty normalized relative path".to_owned(),
            ));
        };
        normalized.push(name);
    }
    if normalized.as_os_str().is_empty() {
        return Err(HostError::Query(
            "workspace path must be a non-empty normalized relative path".to_owned(),
        ));
    }
    Ok(normalized)
}

fn split_virtual_path(value: &str) -> Result<(usize, PathBuf), HostError> {
    let normalized = safe_relative_path(value)?;
    let mut components = normalized.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Err(HostError::Query("workspace path is invalid".to_owned()));
    };
    if first != "@root" {
        return Ok((0, normalized));
    }
    let Some(Component::Normal(index)) = components.next() else {
        return Err(HostError::Query(
            "virtual workspace path must use @root/<index>/...".to_owned(),
        ));
    };
    let index = index
        .to_str()
        .and_then(|index| index.parse::<usize>().ok())
        .filter(|index| *index > 0)
        .ok_or_else(|| HostError::Query("workspace root index must be positive".to_owned()))?;
    let relative = components.fold(PathBuf::new(), |path, component| {
        path.join(component.as_os_str())
    });
    if relative.as_os_str().is_empty() {
        return Err(HostError::Query(
            "virtual workspace path must name a file".to_owned(),
        ));
    }
    Ok((index, relative))
}

#[cfg(unix)]
fn search_workspaces(
    workspaces: &[PathBuf],
    query: &str,
    limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    let mut combined = Vec::new();
    let mut truncated = false;
    for (index, workspace) in workspaces.iter().enumerate() {
        let remaining = limit.saturating_sub(combined.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        let (mut matches, root_truncated) = search_workspace(workspace, query, remaining)?;
        if index > 0 {
            for item in &mut matches {
                item.path = format!("@root/{index}/{}", item.path);
            }
        }
        combined.extend(matches);
        truncated |= root_truncated;
        if truncated || combined.len() >= limit {
            break;
        }
    }
    combined.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((combined, truncated))
}

#[cfg(not(unix))]
fn search_workspaces(
    _workspaces: &[PathBuf],
    _query: &str,
    _limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    Err(HostError::Query(
        "safe workspace search is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
fn preview_file(
    workspace: &Path,
    relative: &Path,
    maximum: usize,
) -> Result<WorkspaceFilePreview, HostError> {
    let root = open_workspace_directory(workspace)?;
    let file = open_relative_regular_file(&root, relative)?;
    let stat = rustix::fs::fstat(&file)
        .map_err(|_| HostError::Query("workspace file metadata is unavailable".to_owned()))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(HostError::Query(
            "workspace preview accepts regular files only".to_owned(),
        ));
    }
    let total_bytes = usize::try_from(stat.st_size).unwrap_or(usize::MAX);
    if total_bytes > maximum {
        return Err(HostError::Query(
            "workspace file exceeds the preview byte limit".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(total_bytes.min(maximum));
    fs::File::from(file)
        .take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| HostError::Query("workspace file could not be read".to_owned()))?;
    if bytes.len() > maximum {
        return Err(HostError::Query(
            "workspace file exceeded the preview byte limit while reading".to_owned(),
        ));
    }
    if bytes.contains(&0) {
        return Err(HostError::Query(
            "binary workspace files are not previewed".to_owned(),
        ));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| HostError::Query("binary workspace files are not previewed".to_owned()))?;
    Ok(WorkspaceFilePreview {
        path: relative.to_string_lossy().into_owned(),
        media_type: "text/plain".to_owned(),
        data: AttachmentData::Text { content },
        total_bytes: u64::try_from(total_bytes).unwrap_or(u64::MAX),
        truncated: false,
    })
}

#[cfg(not(unix))]
fn preview_file(
    _workspace: &Path,
    _relative: &Path,
    _maximum: usize,
) -> Result<WorkspaceFilePreview, HostError> {
    Err(HostError::Query(
        "safe workspace preview is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
fn search_workspace(
    workspace: &Path,
    query: &str,
    limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    let started = Instant::now();
    let needle = query.to_ascii_lowercase();
    let root = open_workspace_directory(workspace)?;
    let mut pending = vec![(root, PathBuf::new())];
    let mut matches = Vec::new();
    let mut visited = 0_usize;
    let mut truncated = false;
    while let Some((directory, relative_directory)) = pending.pop() {
        if started.elapsed() >= QUERY_DEADLINE || visited >= MAX_SEARCH_ENTRIES {
            truncated = true;
            break;
        }
        let entries = rustix::fs::Dir::read_from(&directory)
            .map_err(|_| HostError::Query("workspace directory could not be read".to_owned()))?;
        for entry in entries {
            let entry = entry
                .map_err(|_| HostError::Query("workspace directory read failed".to_owned()))?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let name = std::ffi::OsStr::from_bytes(name.to_bytes());
            let Some(name_text) = name.to_str() else {
                continue;
            };
            visited = visited.saturating_add(1);
            let Ok(child) = rustix::fs::openat(
                &directory,
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            ) else {
                continue;
            };
            let Ok(stat) = rustix::fs::fstat(&child) else {
                continue;
            };
            let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            if !file_type.is_file() && !file_type.is_dir() {
                continue;
            }
            let relative = relative_directory.join(name_text);
            if relative == Path::new(".git") || relative.starts_with(".git") {
                continue;
            }
            let rendered = relative.to_string_lossy().into_owned();
            if needle.is_empty() || rendered.to_ascii_lowercase().contains(&needle) {
                matches.push(WorkspaceFileMatch {
                    path: rendered,
                    is_directory: file_type.is_dir(),
                });
                if matches.len() >= limit {
                    truncated = true;
                    break;
                }
            }
            if file_type.is_dir() {
                pending.push((child, relative));
            }
        }
        if truncated {
            break;
        }
    }
    matches.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((matches, truncated))
}

#[cfg(not(unix))]
fn search_workspace(
    _workspace: &Path,
    _query: &str,
    _limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    Err(HostError::Query(
        "safe workspace search is unavailable on this platform".to_owned(),
    ))
}

fn read_workspace_status(
    workspace: &Path,
    workspace_name: String,
) -> Result<WorkspaceStatus, HostError> {
    let branch = read_git_branch(workspace)?;
    Ok(WorkspaceStatus {
        workspace_name,
        branch,
        changed_paths: Vec::new(),
        truncated: false,
    })
}

#[cfg(unix)]
fn read_git_branch(workspace: &Path) -> Result<Option<String>, HostError> {
    let root = open_workspace_directory(workspace)?;
    let Ok(git) = rustix::fs::openat(
        &root,
        ".git",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
        return Ok(None);
    };
    let Ok(head) = rustix::fs::openat(
        &git,
        "HEAD",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
        return Ok(None);
    };
    let stat = rustix::fs::fstat(&head)
        .map_err(|_| HostError::Query("git HEAD metadata is unavailable".to_owned()))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_size > 4_096 {
        return Ok(None);
    }
    let mut content = String::new();
    fs::File::from(head)
        .take(4_097)
        .read_to_string(&mut content)
        .map_err(|_| HostError::Query("git HEAD could not be read".to_owned()))?;
    let Some(branch) = content.trim().strip_prefix("ref: refs/heads/") else {
        return Ok(None);
    };
    if branch.is_empty()
        || branch
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Ok(None);
    }
    Ok(Some(branch.to_owned()))
}

#[cfg(not(unix))]
fn read_git_branch(_workspace: &Path) -> Result<Option<String>, HostError> {
    Ok(None)
}

#[cfg(unix)]
fn open_workspace_directory(workspace: &Path) -> Result<OwnedFd, HostError> {
    rustix::fs::open(
        workspace,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| HostError::Query("workspace directory could not be opened safely".to_owned()))
}

#[cfg(unix)]
fn open_relative_regular_file(root: &OwnedFd, relative: &Path) -> Result<OwnedFd, HostError> {
    let components = relative.components().collect::<Vec<_>>();
    let mut directory = rustix::fs::openat(
        root,
        ".",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| HostError::Query("workspace directory could not be opened safely".to_owned()))?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(HostError::Query("workspace path is invalid".to_owned()));
        };
        let final_component = index.saturating_add(1) == components.len();
        let mut flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC;
        if !final_component {
            flags |= rustix::fs::OFlags::DIRECTORY;
        }
        let opened = rustix::fs::openat(&directory, *name, flags, rustix::fs::Mode::empty())
            .map_err(|_| {
                HostError::Query("workspace path could not be opened safely".to_owned())
            })?;
        if final_component {
            return Ok(opened);
        }
        directory = opened;
    }
    Err(HostError::Query("workspace path is invalid".to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    #[cfg(unix)]
    use std::time::Instant;

    use tempfile::tempdir;

    use super::*;

    fn factory(root: &Path, workspace: &Path) -> CliSessionFactory {
        CliSessionFactory::new(CliHostOptions {
            storage_root: root.join("state"),
            credentials_path: root.join("state/credentials.json"),
            config: Config::default(),
            allowed_workspaces: vec![workspace.to_path_buf()],
            permission_mode: Some(PermissionMode::Strict),
            max_turns: 2,
            provider_mode: HostedProviderMode::DeterministicReplay {
                provider_name: "offline-host".to_owned(),
                scripts: Vec::new(),
            },
        })
        .expect("factory")
    }

    fn descriptor(workspace: &Path) -> SessionDescriptor {
        SessionDescriptor {
            session_id: SessionId("session-query".to_owned()),
            workspace_name: workspace_name(workspace),
            model: ModelAlias("fast".to_owned()),
            driver_client_id: None,
            shell_active: false,
        }
    }

    #[tokio::test]
    async fn workspace_preview_fails_closed_for_traversal_symlink_and_binary_without_path_leakage()
    {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        fs::write(workspace.join("safe.txt"), "safe").expect("safe file");
        fs::write(workspace.join("binary.bin"), [0, 1, 2]).expect("binary file");
        #[cfg(unix)]
        std::os::unix::fs::symlink("safe.txt", workspace.join("link.txt")).expect("symlink");

        for path in ["../safe.txt", "/etc/passwd"] {
            let error = safe_relative_path(path).expect_err("unsafe relative path");
            assert!(!error.to_string().contains(&workspace.display().to_string()));
        }
        assert_eq!(
            safe_relative_path("nested//safe.txt").expect("normalized path"),
            Path::new("nested/safe.txt")
        );
        assert_eq!(
            split_virtual_path("@root/2/nested/safe.txt").expect("virtual path"),
            (2, PathBuf::from("nested/safe.txt"))
        );
        for path in ["@root/0/file", "@root/1", "@root/1/../escape"] {
            assert!(split_virtual_path(path).is_err(), "{path}");
        }
        for path in ["binary.bin", "link.txt"] {
            let relative = safe_relative_path(path).expect("normalized path");
            let error = preview_file(&workspace, &relative, 1024).expect_err("unsafe preview");
            assert!(!error.to_string().contains(&workspace.display().to_string()));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_preview_rejects_before_opening_under_one_hundred_milliseconds() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let fifo = workspace.join("blocked.fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo fixture")
                .success()
        );
        let started = Instant::now();
        preview_file(&workspace, Path::new("blocked.fifo"), 1024).expect_err("FIFO must fail");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_queries_do_not_escape_during_directory_swap_race() {
        use std::{
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            thread,
        };

        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        let swap = workspace.join("swap");
        let held = workspace.join("held");
        fs::create_dir_all(&swap).expect("safe directory");
        fs::create_dir_all(&outside).expect("outside directory");
        fs::write(swap.join("target.txt"), "SAFE").expect("safe file");
        fs::write(outside.join("target.txt"), "OUTSIDE_CANARY").expect("outside file");
        fs::write(outside.join("OUTSIDE_CANARY.txt"), "outside").expect("outside marker");

        let running = Arc::new(AtomicBool::new(true));
        let attacker_running = Arc::clone(&running);
        let attacker_swap = swap.clone();
        let attacker_held = held.clone();
        let attacker_outside = outside.clone();
        let attacker = thread::spawn(move || {
            while attacker_running.load(Ordering::Relaxed) {
                if fs::rename(&attacker_swap, &attacker_held).is_ok() {
                    std::os::unix::fs::symlink(&attacker_outside, &attacker_swap)
                        .expect("race symlink");
                    fs::remove_file(&attacker_swap).expect("remove race symlink");
                    fs::rename(&attacker_held, &attacker_swap).expect("restore safe directory");
                }
                thread::yield_now();
            }
        });

        for _ in 0..250 {
            if let Ok(preview) = preview_file(&workspace, Path::new("swap/target.txt"), 1024) {
                assert_eq!(
                    preview.data,
                    AttachmentData::Text {
                        content: "SAFE".to_owned()
                    }
                );
            }
            if let Ok((matches, _)) = search_workspace(&workspace, "OUTSIDE_CANARY", 10) {
                assert!(matches.is_empty(), "search escaped through a raced symlink");
            }
        }
        running.store(false, Ordering::Relaxed);
        attacker.join().expect("attacker thread");

        let preview = preview_file(&workspace, Path::new("swap/target.txt"), 1024)
            .expect("safe directory restored");
        assert_eq!(
            preview.data,
            AttachmentData::Text {
                content: "SAFE".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn create_persists_remote_safe_descriptor_and_resume_recovers_exact_identity() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        fs::write(workspace.join("needle.rs"), "fn needle() {}\n").expect("query fixture");
        let factory = factory(root.path(), &workspace);
        let session_id = SessionId("session-create-resume".to_owned());
        let created = factory
            .create(CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("create");
        assert_eq!(created.descriptor().session_id, session_id);
        assert!(!created.descriptor().workspace_name.contains('/'));
        let (matches, truncated) = factory
            .search_workspace_files(&created.descriptor(), "needle", 10)
            .await
            .expect("search");
        assert!(!truncated);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "needle.rs");
        let preview = factory
            .preview_workspace_file(&created.descriptor(), "needle.rs", 1024)
            .await
            .expect("preview");
        assert_eq!(
            preview.data,
            AttachmentData::Text {
                content: "fn needle() {}\n".to_owned()
            }
        );
        drop(created);
        tokio::task::yield_now().await;
        let resumed = factory.resume(&session_id).await.expect("resume");
        assert_eq!(resumed.descriptor().session_id, session_id);
        assert_eq!(resumed.descriptor().workspace_name, "workspace");
    }
}
