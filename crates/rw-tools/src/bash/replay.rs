use std::collections::{BTreeMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::registry::{CancellationToken, ToolError, ToolOutputChunk, ToolOutputSink};

use super::{BashSandboxMode, CommandExecutor, CommandOutcome, CommandRequest};

/// Sanitizes command fixture strings before any request, output, or error is
/// persisted. Production hosts should inject their shared known-secret
/// redactor; the identity implementation is intended for secret-free tests.
pub trait CommandFixtureRedactor: Send + Sync {
    /// Returns a disk-safe replacement for one fixture string.
    fn redact(&self, value: &str) -> String;

    /// Longest registered byte pattern which may span stream chunks.
    fn max_secret_bytes(&self) -> usize {
        0
    }
}

/// Identity command-fixture redactor for secret-free unit fixtures.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityCommandFixtureRedactor;

impl CommandFixtureRedactor for IdentityCommandFixtureRedactor {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}

pub(super) const COMMAND_REPLAY_FILE: &str = "commands.json";
pub(super) const COMMAND_REPLAY_TEMP_FILE: &str = "commands.json.tmp";
pub(super) const COMMAND_REPLAY_ROOT: &str = "${ROTTWEILER_COMMAND_ROOT}";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct CanonicalCommandRequest {
    command: String,
    workspace_relative_cwd: String,
    env: BTreeMap<String, String>,
    #[serde(default)]
    network_domains: Vec<String>,
    #[serde(default)]
    sandbox: BashSandboxMode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum RecordedCommandTerminal {
    Success { outcome: CommandOutcome },
    Cancelled,
    CommandError { message: String },
    OutputError { message: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct RecordedCommandOccurrence {
    request: CanonicalCommandRequest,
    output: Vec<ToolOutputChunk>,
    pub(super) terminal: RecordedCommandTerminal,
}

pub(super) struct RecordingCommandOutput {
    downstream: Arc<dyn ToolOutputSink>,
    chunks: Mutex<Vec<ToolOutputChunk>>,
}

#[async_trait]
impl ToolOutputSink for RecordingCommandOutput {
    async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
        self.downstream.emit(chunk.clone()).await?;
        self.chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(chunk);
        Ok(())
    }
}

/// Command executor middleware that records exact requests, output chunks, and
/// terminal results for deterministic offline replay.
pub struct RecordingCommandExecutor {
    inner: Arc<dyn CommandExecutor>,
    workspace_root: PathBuf,
    fixture_path: PathBuf,
    occurrences: Mutex<Vec<RecordedCommandOccurrence>>,
    redactor: Arc<dyn CommandFixtureRedactor>,
    run_lock: tokio::sync::Mutex<()>,
}

impl RecordingCommandExecutor {
    /// Opens a recording middleware rooted at one workspace and fixture directory.
    ///
    /// # Errors
    ///
    /// Returns an error if either root cannot be prepared or an existing fixture
    /// is malformed.
    pub fn new(
        inner: Arc<dyn CommandExecutor>,
        fixture_directory: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ToolError> {
        Self::new_with_redactor(
            inner,
            fixture_directory,
            workspace_root,
            Arc::new(IdentityCommandFixtureRedactor),
        )
    }

    /// Opens recording middleware with an injected known-secret redactor.
    ///
    /// # Errors
    ///
    /// Returns an error if either root cannot be prepared or an existing fixture
    /// is malformed.
    pub fn new_with_redactor(
        inner: Arc<dyn CommandExecutor>,
        fixture_directory: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
        redactor: Arc<dyn CommandFixtureRedactor>,
    ) -> Result<Self, ToolError> {
        let workspace_root =
            std::fs::canonicalize(workspace_root.as_ref()).map_err(|source| ToolError::Io {
                operation: "canonicalize command replay workspace",
                path: workspace_root.as_ref().to_path_buf(),
                source,
            })?;
        std::fs::create_dir_all(fixture_directory.as_ref()).map_err(|source| ToolError::Io {
            operation: "create command replay directory",
            path: fixture_directory.as_ref().to_path_buf(),
            source,
        })?;
        let fixture_path = fixture_directory.as_ref().join(COMMAND_REPLAY_FILE);
        let occurrences = load_command_occurrences(&fixture_path)?;
        Ok(Self {
            inner,
            workspace_root,
            fixture_path,
            occurrences: Mutex::new(occurrences),
            redactor,
            run_lock: tokio::sync::Mutex::new(()),
        })
    }
}

#[async_trait]
impl CommandExecutor for RecordingCommandExecutor {
    async fn settle_effects(&self) {
        self.inner.settle_effects().await;
    }
    fn supports_background(&self) -> bool {
        false
    }

    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        let _run = self.run_lock.lock().await;
        let canonical = canonical_command_request(&self.workspace_root, &request)?;
        let recording_output = Arc::new(RecordingCommandOutput {
            downstream: output,
            chunks: Mutex::new(Vec::new()),
        });
        let result = self
            .inner
            .run(request, cancellation, recording_output.clone())
            .await;
        let terminal = recorded_terminal(&result);
        let occurrence = redact_command_occurrence(
            RecordedCommandOccurrence {
                request: canonical,
                output: recording_output
                    .chunks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                terminal,
            },
            self.redactor.as_ref(),
        );
        {
            let mut occurrences = self
                .occurrences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            occurrences.push(occurrence);
            persist_command_occurrences(&self.fixture_path, &occurrences)?;
        }
        result
    }
}

/// Offline command executor that serves exact recorded occurrences and never
/// spawns a process or opens a socket.
pub struct ReplayCommandExecutor {
    workspace_root: PathBuf,
    occurrences: tokio::sync::Mutex<VecDeque<RecordedCommandOccurrence>>,
}

impl ReplayCommandExecutor {
    /// Creates an empty fail-closed replay executor for offline sessions which
    /// have no command fixture directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the replay workspace cannot be canonicalized.
    pub fn empty(workspace_root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let workspace_root =
            std::fs::canonicalize(workspace_root.as_ref()).map_err(|source| ToolError::Io {
                operation: "canonicalize command replay workspace",
                path: workspace_root.as_ref().to_path_buf(),
                source,
            })?;
        Ok(Self {
            workspace_root,
            occurrences: tokio::sync::Mutex::new(VecDeque::new()),
        })
    }

    /// Loads deterministic command occurrences for one replay workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid workspace or malformed/missing fixture.
    pub fn load(
        fixture_directory: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ToolError> {
        let workspace_root =
            std::fs::canonicalize(workspace_root.as_ref()).map_err(|source| ToolError::Io {
                operation: "canonicalize command replay workspace",
                path: workspace_root.as_ref().to_path_buf(),
                source,
            })?;
        let fixture_path = fixture_directory.as_ref().join(COMMAND_REPLAY_FILE);
        let occurrences = load_command_occurrences(&fixture_path)?;
        Ok(Self {
            workspace_root,
            occurrences: tokio::sync::Mutex::new(occurrences.into()),
        })
    }
}

#[async_trait]
impl CommandExecutor for ReplayCommandExecutor {
    async fn settle_effects(&self) {}
    fn supports_background(&self) -> bool {
        false
    }

    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        cancellation.check()?;
        let canonical = canonical_command_request(&self.workspace_root, &request)?;
        let occurrence =
            self.occurrences.lock().await.pop_front().ok_or_else(|| {
                ToolError::Command("command replay fixture is exhausted".to_owned())
            })?;
        if occurrence.request != canonical {
            return Err(ToolError::Command(
                "command replay request did not match the next recorded occurrence".to_owned(),
            ));
        }
        for chunk in occurrence.output {
            cancellation.check()?;
            output.emit(chunk).await?;
        }
        match occurrence.terminal {
            RecordedCommandTerminal::Success { outcome } => Ok(outcome),
            RecordedCommandTerminal::Cancelled => Err(ToolError::Cancelled),
            RecordedCommandTerminal::CommandError { message } => Err(ToolError::Command(message)),
            RecordedCommandTerminal::OutputError { message } => Err(ToolError::Output(message)),
        }
    }
}

pub(super) fn canonical_command_request(
    workspace_root: &Path,
    request: &CommandRequest,
) -> Result<CanonicalCommandRequest, ToolError> {
    let cwd = std::fs::canonicalize(&request.cwd).map_err(|source| ToolError::Io {
        operation: "canonicalize command replay cwd",
        path: request.cwd.clone(),
        source,
    })?;
    let relative = cwd.strip_prefix(workspace_root).map_err(|_| {
        ToolError::Command("command cwd is outside the replay workspace".to_owned())
    })?;
    let workspace_relative_cwd = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let env = request
        .env
        .iter()
        .map(|(name, value)| {
            let value_path = Path::new(value);
            let points_to_root = value_path.is_absolute()
                && std::fs::canonicalize(value_path).is_ok_and(|path| path == workspace_root);
            let value = if points_to_root {
                COMMAND_REPLAY_ROOT.to_owned()
            } else {
                value.clone()
            };
            (name.clone(), value)
        })
        .collect();
    Ok(CanonicalCommandRequest {
        command: request.command.clone(),
        workspace_relative_cwd,
        env,
        network_domains: request.network_domains.clone(),
        sandbox: request.sandbox,
    })
}

pub(super) fn recorded_terminal(
    result: &Result<CommandOutcome, ToolError>,
) -> RecordedCommandTerminal {
    match result {
        Ok(outcome) => RecordedCommandTerminal::Success { outcome: *outcome },
        Err(ToolError::Cancelled) => RecordedCommandTerminal::Cancelled,
        Err(ToolError::Output(message)) => RecordedCommandTerminal::OutputError {
            message: message.clone(),
        },
        Err(error) => RecordedCommandTerminal::CommandError {
            message: error.to_string(),
        },
    }
}

pub(super) fn redact_command_occurrence(
    mut occurrence: RecordedCommandOccurrence,
    redactor: &dyn CommandFixtureRedactor,
) -> RecordedCommandOccurrence {
    occurrence.request.command = redactor.redact(&occurrence.request.command);
    occurrence.request.workspace_relative_cwd =
        redactor.redact(&occurrence.request.workspace_relative_cwd);
    occurrence.request.env = occurrence
        .request
        .env
        .into_iter()
        .map(|(name, value)| (redactor.redact(&name), redactor.redact(&value)))
        .collect();
    for chunk in &mut occurrence.output {
        chunk.content = redactor.redact(&chunk.content);
    }
    match &mut occurrence.terminal {
        RecordedCommandTerminal::CommandError { message }
        | RecordedCommandTerminal::OutputError { message } => {
            *message = redactor.redact(message);
        }
        RecordedCommandTerminal::Success { .. } | RecordedCommandTerminal::Cancelled => {}
    }
    occurrence
}

pub(super) fn load_command_occurrences(
    path: &Path,
) -> Result<Vec<RecordedCommandOccurrence>, ToolError> {
    #[cfg(unix)]
    {
        load_command_occurrences_unix(path)
    }
    #[cfg(not(unix))]
    {
        load_command_occurrences_portable(path)
    }
}

pub(super) fn persist_command_occurrences(
    path: &Path,
    occurrences: &[RecordedCommandOccurrence],
) -> Result<(), ToolError> {
    let bytes = serde_json::to_vec_pretty(occurrences)
        .map_err(|error| ToolError::Command(format!("command replay could not encode: {error}")))?;
    #[cfg(unix)]
    {
        persist_command_occurrences_unix(path, &bytes)
    }
    #[cfg(not(unix))]
    {
        persist_command_occurrences_portable(path, &bytes)
    }
}

pub(super) fn decode_command_occurrences(
    bytes: &[u8],
) -> Result<Vec<RecordedCommandOccurrence>, ToolError> {
    serde_json::from_slice(bytes).map_err(|error| {
        ToolError::Command(format!("command replay fixture is malformed: {error}"))
    })
}

#[cfg(unix)]
pub(super) fn command_fixture_directory(path: &Path) -> Result<std::os::fd::OwnedFd, ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::Command("command replay fixture has no parent".to_owned()))?;
    rustix::fs::open(
        parent,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ToolError::Io {
        operation: "open command replay directory",
        path: parent.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
pub(super) fn read_private_regular_at(
    directory: &std::os::fd::OwnedFd,
    name: &str,
    path: &Path,
) -> Result<Option<Vec<u8>>, ToolError> {
    let stat = match rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => {
            return Err(ToolError::Io {
                operation: "inspect command replay fixture",
                path: path.to_path_buf(),
                source: std::io::Error::from(error),
            });
        }
    };
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(ToolError::Command(
            "command replay fixture must be a regular file, never a symlink or special file"
                .to_owned(),
        ));
    }
    if stat.st_mode & 0o077 != 0 {
        return Err(ToolError::Command(
            "command replay fixture permissions must not grant group or other access".to_owned(),
        ));
    }
    let descriptor = rustix::fs::openat(
        directory,
        name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ToolError::Io {
        operation: "open command replay fixture",
        path: path.to_path_buf(),
        source,
    })?;
    let mut file = std::fs::File::from(descriptor);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| ToolError::Io {
            operation: "read command replay fixture",
            path: path.to_path_buf(),
            source,
        })?;
    Ok(Some(bytes))
}

#[cfg(unix)]
pub(super) fn load_command_occurrences_unix(
    path: &Path,
) -> Result<Vec<RecordedCommandOccurrence>, ToolError> {
    let directory = command_fixture_directory(path)?;
    let temporary_path = path.with_file_name(COMMAND_REPLAY_TEMP_FILE);
    let installed = read_private_regular_at(&directory, COMMAND_REPLAY_FILE, path)?;
    let temporary = read_private_regular_at(&directory, COMMAND_REPLAY_TEMP_FILE, &temporary_path)?;
    match (installed, temporary) {
        (Some(bytes), Some(_)) => {
            let decoded = decode_command_occurrences(&bytes)?;
            rustix::fs::unlinkat(
                &directory,
                COMMAND_REPLAY_TEMP_FILE,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(std::io::Error::from)
            .map_err(|source| ToolError::Io {
                operation: "remove stale command replay temporary",
                path: temporary_path,
                source,
            })?;
            sync_command_fixture_directory(&directory, path)?;
            Ok(decoded)
        }
        (Some(bytes), None) => decode_command_occurrences(&bytes),
        (None, Some(bytes)) => {
            let decoded = decode_command_occurrences(&bytes)?;
            rustix::fs::renameat(
                &directory,
                COMMAND_REPLAY_TEMP_FILE,
                &directory,
                COMMAND_REPLAY_FILE,
            )
            .map_err(std::io::Error::from)
            .map_err(|source| ToolError::Io {
                operation: "recover command replay temporary",
                path: path.to_path_buf(),
                source,
            })?;
            sync_command_fixture_directory(&directory, path)?;
            Ok(decoded)
        }
        (None, None) => Ok(Vec::new()),
    }
}

#[cfg(unix)]
pub(super) fn persist_command_occurrences_unix(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let directory = command_fixture_directory(path)?;
    let _ = read_private_regular_at(&directory, COMMAND_REPLAY_FILE, path)?;
    let temporary_path = path.with_file_name(COMMAND_REPLAY_TEMP_FILE);
    if read_private_regular_at(&directory, COMMAND_REPLAY_TEMP_FILE, &temporary_path)?.is_some() {
        rustix::fs::unlinkat(
            &directory,
            COMMAND_REPLAY_TEMP_FILE,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "remove stale command replay temporary",
            path: temporary_path.clone(),
            source,
        })?;
        sync_command_fixture_directory(&directory, path)?;
    }
    let descriptor = rustix::fs::openat(
        &directory,
        COMMAND_REPLAY_TEMP_FILE,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ToolError::Io {
        operation: "create command replay temporary",
        path: temporary_path.clone(),
        source,
    })?;
    let mut file = std::fs::File::from(descriptor);
    let installed = (|| -> Result<(), ToolError> {
        file.write_all(bytes).map_err(|source| ToolError::Io {
            operation: "write command replay temporary",
            path: temporary_path.clone(),
            source,
        })?;
        file.flush().map_err(|source| ToolError::Io {
            operation: "flush command replay temporary",
            path: temporary_path.clone(),
            source,
        })?;
        rustix::fs::fsync(&file)
            .map_err(std::io::Error::from)
            .map_err(|source| ToolError::Io {
                operation: "synchronize command replay temporary",
                path: temporary_path.clone(),
                source,
            })?;
        rustix::fs::renameat(
            &directory,
            COMMAND_REPLAY_TEMP_FILE,
            &directory,
            COMMAND_REPLAY_FILE,
        )
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "install command replay fixture",
            path: path.to_path_buf(),
            source,
        })?;
        sync_command_fixture_directory(&directory, path)
    })();
    if installed.is_err() {
        let _ = rustix::fs::unlinkat(
            &directory,
            COMMAND_REPLAY_TEMP_FILE,
            rustix::fs::AtFlags::empty(),
        );
    }
    installed
}

#[cfg(unix)]
pub(super) fn sync_command_fixture_directory(
    directory: &std::os::fd::OwnedFd,
    path: &Path,
) -> Result<(), ToolError> {
    rustix::fs::fsync(directory)
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "synchronize command replay directory",
            path: path.parent().unwrap_or(path).to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
pub(super) fn load_command_occurrences_portable(
    path: &Path,
) -> Result<Vec<RecordedCommandOccurrence>, ToolError> {
    let temporary = path.with_file_name(COMMAND_REPLAY_TEMP_FILE);
    reject_non_regular_portable(path)?;
    reject_non_regular_portable(&temporary)?;
    match (std::fs::read(path), std::fs::read(&temporary)) {
        (Ok(bytes), Ok(_)) => {
            let decoded = decode_command_occurrences(&bytes)?;
            std::fs::remove_file(&temporary).map_err(|source| ToolError::Io {
                operation: "remove stale command replay temporary",
                path: temporary,
                source,
            })?;
            Ok(decoded)
        }
        (Ok(bytes), Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            decode_command_occurrences(&bytes)
        }
        (Err(error), Ok(bytes)) if error.kind() == std::io::ErrorKind::NotFound => {
            let decoded = decode_command_occurrences(&bytes)?;
            std::fs::rename(&temporary, path).map_err(|source| ToolError::Io {
                operation: "recover command replay temporary",
                path: path.to_path_buf(),
                source,
            })?;
            Ok(decoded)
        }
        (Err(left), Err(right))
            if left.kind() == std::io::ErrorKind::NotFound
                && right.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(Vec::new())
        }
        (Err(source), _) | (_, Err(source)) => Err(ToolError::Io {
            operation: "read command replay fixture",
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(not(unix))]
pub(super) fn persist_command_occurrences_portable(
    path: &Path,
    bytes: &[u8],
) -> Result<(), ToolError> {
    use std::fs::OpenOptions;

    let temporary = path.with_file_name(COMMAND_REPLAY_TEMP_FILE);
    reject_non_regular_portable(path)?;
    reject_non_regular_portable(&temporary)?;
    if temporary.exists() {
        std::fs::remove_file(&temporary).map_err(|source| ToolError::Io {
            operation: "remove stale command replay temporary",
            path: temporary.clone(),
            source,
        })?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|source| ToolError::Io {
            operation: "create command replay temporary",
            path: temporary.clone(),
            source,
        })?;
    file.write_all(bytes).map_err(|source| ToolError::Io {
        operation: "write command replay temporary",
        path: temporary.clone(),
        source,
    })?;
    file.sync_all().map_err(|source| ToolError::Io {
        operation: "synchronize command replay temporary",
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| ToolError::Io {
        operation: "install command replay fixture",
        path: path.to_path_buf(),
        source,
    })?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ToolError::Io {
                operation: "synchronize command replay directory",
                path: parent.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn reject_non_regular_portable(path: &Path) -> Result<(), ToolError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(ToolError::Command(
            "command replay fixture must be a regular file, never a symlink or special file"
                .to_owned(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ToolError::Io {
            operation: "inspect command replay fixture",
            path: path.to_path_buf(),
            source,
        }),
    }
}
