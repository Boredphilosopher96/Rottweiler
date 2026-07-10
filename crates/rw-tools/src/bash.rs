use std::collections::{BTreeMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rw_types::{ToolCapability, ToolOutputStream};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::{Duration, sleep};

use crate::registry::{
    CancellationToken, CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError,
    ToolLimits, ToolOutputChunk, ToolOutputSink, ToolResult, input_schema, parse_input,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashInput {
    pub command: String,
    #[serde(default = "default_cwd")]
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_cwd() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest {
    pub command: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandOutcome {
    pub exit_code: i32,
}

/// Injected process boundary. Core must approve the bash manifest before this is called.
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError>;
}

/// A data-less, inheritable session lease used to order crash recovery after
/// process-group cleanup.
///
/// On Unix the parent descriptor uses `CLOEXEC`. Only the parent-death
/// watchdog receives a duplicate mapped to one of its standard descriptors;
/// arbitrary commands never inherit a usable lease descriptor. A replacement
/// session therefore cannot recover checkpoints until the watchdog has killed
/// and observed the command group exit. The event log is separate and is never
/// exposed.
#[derive(Debug)]
pub struct ExecutionLease {
    file: std::fs::File,
}

impl ExecutionLease {
    /// Opens and exclusively locks a private regular lease file.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or unsafe parent directory, an unsafe
    /// lease file, insecure permissions, or a lock failure.
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, ToolError> {
        acquire_execution_lease(path.as_ref())
    }

    #[cfg(unix)]
    fn watchdog_stdio(&self) -> Result<Stdio, ToolError> {
        self.file
            .try_clone()
            .map(Stdio::from)
            .map_err(|source| ToolError::Io {
                operation: "duplicate execution lease for watchdog",
                path: PathBuf::from("execution.lock"),
                source,
            })
    }

    #[cfg(all(unix, test))]
    fn test_watchdog_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.file)
    }
}

#[cfg(unix)]
fn acquire_execution_lease(path: &Path) -> Result<ExecutionLease, ToolError> {
    let parent_path = path
        .parent()
        .ok_or_else(|| ToolError::Command("execution lease has no parent".to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| ToolError::Command("execution lease has no file name".to_owned()))?;
    let parent = rustix::fs::open(
        parent_path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ToolError::Io {
        operation: "open execution lease directory",
        path: parent_path.to_path_buf(),
        source,
    })?;
    let parent_stat = rustix::fs::fstat(&parent)
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "inspect execution lease directory",
            path: parent_path.to_path_buf(),
            source,
        })?;
    if !rustix::fs::FileType::from_raw_mode(parent_stat.st_mode).is_dir()
        || parent_stat.st_uid != rustix::process::geteuid().as_raw()
        || parent_stat.st_mode & 0o022 != 0
    {
        return Err(ToolError::Command(
            "execution lease directory must be owner-controlled and not group/other writable"
                .to_owned(),
        ));
    }
    let descriptor = rustix::fs::openat(
        &parent,
        file_name,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ToolError::Io {
        operation: "open execution lease",
        path: path.to_path_buf(),
        source,
    })?;
    let file = std::fs::File::from(descriptor);
    let stat = rustix::fs::fstat(&file)
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "inspect execution lease",
            path: path.to_path_buf(),
            source,
        })?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(ToolError::Command(
            "execution lease must be a regular file, never a symlink or special file".to_owned(),
        ));
    }
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(ToolError::Command(
            "execution lease must be owned by the current user".to_owned(),
        ));
    }
    if stat.st_mode & 0o077 != 0 {
        return Err(ToolError::Command(
            "execution lease permissions must not grant group or other access".to_owned(),
        ));
    }
    loop {
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
            Ok(()) => break,
            Err(rustix::io::Errno::INTR) => {}
            Err(source) => {
                return Err(ToolError::Io {
                    operation: "lock execution lease",
                    path: path.to_path_buf(),
                    source: std::io::Error::from(source),
                });
            }
        }
    }
    rustix::fs::fsync(&parent)
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "synchronize execution lease directory",
            path: parent_path.to_path_buf(),
            source,
        })?;
    Ok(ExecutionLease { file })
}

#[cfg(not(unix))]
fn acquire_execution_lease(path: &Path) -> Result<ExecutionLease, ToolError> {
    Err(ToolError::Command(format!(
        "execution leases are unavailable on this platform; refusing unlocked session startup at {}",
        path.display()
    )))
}

/// Sanitizes command fixture strings before any request, output, or error is
/// persisted. Production hosts should inject their shared known-secret
/// redactor; the identity implementation is intended for secret-free tests.
pub trait CommandFixtureRedactor: Send + Sync {
    /// Returns a disk-safe replacement for one fixture string.
    fn redact(&self, value: &str) -> String;
}

/// Identity command-fixture redactor for secret-free unit fixtures.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityCommandFixtureRedactor;

impl CommandFixtureRedactor for IdentityCommandFixtureRedactor {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}

const COMMAND_REPLAY_FILE: &str = "commands.json";
const COMMAND_REPLAY_TEMP_FILE: &str = "commands.json.tmp";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CanonicalCommandRequest {
    command: String,
    workspace_relative_cwd: String,
    env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RecordedCommandTerminal {
    Success { outcome: CommandOutcome },
    Cancelled,
    CommandError { message: String },
    OutputError { message: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct RecordedCommandOccurrence {
    request: CanonicalCommandRequest,
    output: Vec<ToolOutputChunk>,
    terminal: RecordedCommandTerminal,
}

struct RecordingCommandOutput {
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

fn canonical_command_request(
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
    Ok(CanonicalCommandRequest {
        command: request.command.clone(),
        workspace_relative_cwd,
        env: request.env.clone(),
    })
}

fn recorded_terminal(result: &Result<CommandOutcome, ToolError>) -> RecordedCommandTerminal {
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

fn redact_command_occurrence(
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

fn load_command_occurrences(path: &Path) -> Result<Vec<RecordedCommandOccurrence>, ToolError> {
    #[cfg(unix)]
    {
        load_command_occurrences_unix(path)
    }
    #[cfg(not(unix))]
    {
        load_command_occurrences_portable(path)
    }
}

fn persist_command_occurrences(
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

fn decode_command_occurrences(bytes: &[u8]) -> Result<Vec<RecordedCommandOccurrence>, ToolError> {
    serde_json::from_slice(bytes).map_err(|error| {
        ToolError::Command(format!("command replay fixture is malformed: {error}"))
    })
}

#[cfg(unix)]
fn command_fixture_directory(path: &Path) -> Result<std::os::fd::OwnedFd, ToolError> {
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
fn read_private_regular_at(
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
fn load_command_occurrences_unix(path: &Path) -> Result<Vec<RecordedCommandOccurrence>, ToolError> {
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
fn persist_command_occurrences_unix(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
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
fn sync_command_fixture_directory(
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
fn load_command_occurrences_portable(
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
fn persist_command_occurrences_portable(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
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
fn reject_non_regular_portable(path: &Path) -> Result<(), ToolError> {
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

/// Un-sandboxed M2 executor. Its tool manifest explicitly declares every ambient capability.
#[derive(Clone, Debug, Default)]
pub struct TokioCommandExecutor {
    execution_lease: Option<Arc<ExecutionLease>>,
}

impl TokioCommandExecutor {
    /// Retains the session execution lease for this process boundary.
    #[must_use]
    pub fn with_execution_lease(execution_lease: Arc<ExecutionLease>) -> Self {
        Self {
            execution_lease: Some(execution_lease),
        }
    }
}

#[async_trait]
impl CommandExecutor for TokioCommandExecutor {
    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        // Keep the deliberately inheritable lease descriptor alive while the
        // command and its watchdog are spawned.
        let _execution_lease = self.execution_lease.as_ref();
        cancellation.check()?;
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("IFS= read -r _ || exit 125; exec /bin/sh -lc \"$1\"")
            .arg("rottweiler-command-launcher")
            .arg(&request.command)
            .current_dir(&request.cwd)
            .envs(&request.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Command(error.to_string()))?;
        let child_id = child.id();
        let mut launch_gate = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Command("command launch gate was not created".to_owned()))?;
        let mut watchdog =
            match spawn_parent_death_watchdog(child_id, self.execution_lease.as_deref()).await {
                Ok(watchdog) => watchdog,
                Err(error) => {
                    terminate_process_group(child_id);
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    terminate_and_wait_process_group(child_id).await?;
                    return Err(error);
                }
            };
        if let Err(error) = launch_gate.write_all(b"armed\n").await {
            terminate_process_group(child_id);
            let _ = child.start_kill();
            let _ = child.wait().await;
            terminate_and_wait_process_group(child_id).await?;
            let _ = watchdog.disarm().await;
            return Err(ToolError::Command(format!(
                "could not release guarded command: {error}"
            )));
        }
        let _ = launch_gate.shutdown().await;
        drop(launch_gate);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Command("stdout pipe was not created".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Command("stderr pipe was not created".to_owned()))?;
        let stdout_task = tokio::spawn(copy_stream(
            stdout,
            ToolOutputStream::Stdout,
            Arc::clone(&output),
        ));
        let stderr_task = tokio::spawn(copy_stream(stderr, ToolOutputStream::Stderr, output));

        let status = tokio::select! {
            status = child.wait() => status,
            watchdog_status = watchdog.wait_unexpected() => {
                terminate_process_group(child_id);
                let _ = child.start_kill();
                let _ = child.wait().await;
                terminate_and_wait_process_group(child_id).await?;
                finish_output_task(stdout_task).await?;
                finish_output_task(stderr_task).await?;
                return Err(ToolError::Command(format!(
                    "command watchdog exited before command completion: {watchdog_status}"
                )));
            }
            () = cancellation.cancelled() => {
                terminate_process_group(child_id);
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                terminate_and_wait_process_group(child_id).await?;
                watchdog.disarm().await?;
                finish_output_task(stdout_task).await?;
                finish_output_task(stderr_task).await?;
                return Err(ToolError::Cancelled);
            }
        };
        terminate_and_wait_process_group(child_id).await?;
        watchdog.disarm().await?;
        finish_output_task(stdout_task).await?;
        finish_output_task(stderr_task).await?;
        let status = status.map_err(|error| ToolError::Command(error.to_string()))?;
        Ok(CommandOutcome {
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

#[cfg(unix)]
struct ParentDeathWatchdog {
    child: Child,
    control: Option<ChildStdin>,
    stderr_task: tokio::task::JoinHandle<std::io::Result<u64>>,
}

#[cfg(unix)]
async fn spawn_parent_death_watchdog(
    command_group_id: Option<u32>,
    execution_lease: Option<&ExecutionLease>,
) -> Result<ParentDeathWatchdog, ToolError> {
    let group_id = command_group_id
        .ok_or_else(|| ToolError::Command("command process id was unavailable".to_owned()))?;
    let script = r#"
if ! : >&1; then exit 126; fi
printf 'ready\n' >&2
if [ -n "$2" ]; then printf '%s\n' "$$" > "$2"; fi
if IFS= read -r _; then exit 0; fi
if [ -n "$3" ]; then while [ -e "$3" ]; do sleep 0.01; done; fi
kill -KILL -- "-$1" 2>/dev/null || :
while kill -0 -- "-$1" 2>/dev/null; do sleep 0.01; done
"#;
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .arg("rottweiler-parent-death-watchdog")
        .arg(group_id.to_string())
        .arg(watchdog_test_pid_file())
        .arg(watchdog_test_pause_file())
        .stdin(Stdio::piped())
        .stdout(match execution_lease {
            Some(execution_lease) => execution_lease.watchdog_stdio()?,
            None => Stdio::null(),
        })
        .stderr(Stdio::piped());
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        ToolError::Command(format!("could not start command watchdog: {error}"))
    })?;
    let control = child
        .stdin
        .take()
        .ok_or_else(|| ToolError::Command("watchdog control pipe was not created".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Command("watchdog ready pipe was not created".to_owned()))?;
    let mut stderr = BufReader::new(stderr);
    let mut readiness = String::new();
    let ready =
        tokio::time::timeout(Duration::from_secs(2), stderr.read_line(&mut readiness)).await;
    if !matches!(ready, Ok(Ok(_))) || readiness != "ready\n" {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(ToolError::Command(
            "command watchdog did not confirm its execution lease".to_owned(),
        ));
    }
    let stderr_task = tokio::spawn(async move {
        let mut sink = tokio::io::sink();
        tokio::io::copy(&mut stderr, &mut sink).await
    });
    Ok(ParentDeathWatchdog {
        child,
        control: Some(control),
        stderr_task,
    })
}

#[cfg(all(unix, test))]
fn watchdog_test_pid_file() -> String {
    std::env::var("ROTTWEILER_WATCHDOG_TEST_PID_FILE").unwrap_or_default()
}

#[cfg(all(unix, test))]
fn watchdog_test_pause_file() -> String {
    std::env::var("ROTTWEILER_WATCHDOG_PAUSE_FILE").unwrap_or_default()
}

#[cfg(all(unix, not(test)))]
fn watchdog_test_pid_file() -> String {
    String::new()
}

#[cfg(all(unix, not(test)))]
fn watchdog_test_pause_file() -> String {
    String::new()
}

#[cfg(unix)]
impl ParentDeathWatchdog {
    async fn wait_unexpected(&mut self) -> String {
        match self.child.wait().await {
            Ok(status) => status.to_string(),
            Err(error) => error.to_string(),
        }
    }

    async fn disarm(&mut self) -> Result<(), ToolError> {
        if let Some(mut control) = self.control.take() {
            control.write_all(b"done\n").await.map_err(|error| {
                ToolError::Command(format!("could not disarm watchdog: {error}"))
            })?;
            let _ = control.shutdown().await;
        }
        let result = match tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(ToolError::Command(format!(
                "command watchdog failed while disarming: {status}"
            ))),
            Ok(Err(error)) => Err(ToolError::Command(format!(
                "could not reap command watchdog: {error}"
            ))),
            Err(_) => {
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
                Err(ToolError::Command(
                    "command watchdog did not terminate after disarm".to_owned(),
                ))
            }
        };
        let _ = (&mut self.stderr_task).await;
        result
    }
}

#[cfg(not(unix))]
struct ParentDeathWatchdog;

#[cfg(not(unix))]
async fn spawn_parent_death_watchdog(
    _command_group_id: Option<u32>,
    _execution_lease: Option<&ExecutionLease>,
) -> Result<ParentDeathWatchdog, ToolError> {
    Ok(ParentDeathWatchdog)
}

#[cfg(not(unix))]
impl ParentDeathWatchdog {
    async fn wait_unexpected(&mut self) -> String {
        std::future::pending().await
    }

    async fn disarm(&mut self) -> Result<(), ToolError> {
        Ok(())
    }
}

async fn copy_stream(
    mut reader: impl AsyncRead + Unpin,
    stream: ToolOutputStream,
    output: Arc<dyn ToolOutputSink>,
) -> Result<(), ToolError> {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| ToolError::Output(error.to_string()))?;
        if read == 0 {
            return Ok(());
        }
        output
            .emit(ToolOutputChunk {
                stream: stream.clone(),
                content: String::from_utf8_lossy(&buffer[..read]).into_owned(),
            })
            .await?;
    }
}

async fn finish_output_task(
    mut task: tokio::task::JoinHandle<Result<(), ToolError>>,
) -> Result<(), ToolError> {
    tokio::select! {
        result = &mut task => result.map_err(|error| ToolError::Output(error.to_string()))?,
        () = sleep(Duration::from_secs(2)) => {
            task.abort();
            Ok(())
        }
    }
}

#[cfg(unix)]
fn terminate_process_group(child_id: Option<u32>) {
    let Some(raw_pid) = child_id.and_then(|id| i32::try_from(id).ok()) else {
        return;
    };
    if let Some(pid) = rustix::process::Pid::from_raw(raw_pid) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(unix)]
async fn terminate_and_wait_process_group(child_id: Option<u32>) -> Result<(), ToolError> {
    let raw_pid = child_id
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| ToolError::Command("command process group id was unavailable".to_owned()))?;
    let _ = rustix::process::kill_process_group(raw_pid, rustix::process::Signal::KILL);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match rustix::process::test_kill_process_group(raw_pid) {
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            Ok(()) => {}
            Err(error) => {
                return Err(ToolError::Command(format!(
                    "could not verify command process-group exit: {error}"
                )));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            // Returning would allow the opaque-checkpoint post-scan to race a
            // surviving group member. Keep the operation pending and the
            // watchdog/lease armed: this is the fail-closed state.
            std::future::pending::<()>().await;
            unreachable!("pending process-group barrier cannot complete");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_child_id: Option<u32>) {}

#[cfg(not(unix))]
async fn terminate_and_wait_process_group(_child_id: Option<u32>) -> Result<(), ToolError> {
    Err(ToolError::Command(
        "process-group exit barriers are unavailable on this platform".to_owned(),
    ))
}

#[derive(Clone)]
pub struct BashTool {
    executor: Arc<dyn CommandExecutor>,
    limits: ToolLimits,
}

impl BashTool {
    #[must_use]
    pub fn new(executor: Arc<dyn CommandExecutor>, limits: ToolLimits) -> Self {
        Self { executor, limits }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "bash".to_owned(),
            description: "Run an unsandboxed shell command with live stdout/stderr streaming."
                .to_owned(),
            input_schema: input_schema::<BashInput>(),
            capabilities: CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
                ToolCapability::Network,
                ToolCapability::Execute,
            ]),
        }
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: BashInput = parse_input(input)?;
        if input.command.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "command must not be empty".to_owned(),
            ));
        }
        let cwd = context.resolve_existing(&input.cwd)?;
        if !cwd.is_dir() {
            return Err(ToolError::InvalidInput(
                "command cwd must be a directory".to_owned(),
            ));
        }
        let framing_reserve = self.limits.max_result_bytes.min(512) / 4;
        let capture = Arc::new(CapturingSink::new(
            Arc::clone(&context.output),
            self.limits.max_result_bytes.saturating_sub(framing_reserve),
        ));
        let outcome = self
            .executor
            .run(
                CommandRequest {
                    command: input.command,
                    cwd,
                    env: input.env,
                },
                context.cancellation.clone(),
                capture.clone(),
            )
            .await?;
        context.cancellation.check()?;
        let captured = capture.finish()?;
        let model_text = format!(
            "exit code: {}\nstdout:\n{}\nstderr:\n{}",
            outcome.exit_code, captured.stdout, captured.stderr
        );
        let mut result = ToolResult::new(
            model_text,
            json!({
                "exit_code": outcome.exit_code,
                "stdout_truncated": captured.stdout_truncated,
                "stderr_truncated": captured.stderr_truncated,
            }),
        );
        result.truncated = captured.stdout_truncated || captured.stderr_truncated;
        Ok(result)
    }
}

struct CapturingSink {
    upstream: Arc<dyn ToolOutputSink>,
    state: Mutex<CapturedState>,
}

struct CapturedState {
    stdout: TailBuffer,
    stderr: TailBuffer,
    limit: usize,
}

impl CapturingSink {
    fn new(upstream: Arc<dyn ToolOutputSink>, limit: usize) -> Self {
        Self {
            upstream,
            state: Mutex::new(CapturedState {
                stdout: TailBuffer::new(limit),
                stderr: TailBuffer::new(limit),
                limit,
            }),
        }
    }

    fn finish(&self) -> Result<CapturedOutput, ToolError> {
        let state = self
            .state
            .lock()
            .map_err(|_| ToolError::Output("capture lock was poisoned".to_owned()))?;
        let stdout_seen = state.stdout.total_seen;
        let stderr_seen = state.stderr.total_seen;
        let (stdout_limit, stderr_limit) =
            allocate_stream_limits(state.limit, stdout_seen, stderr_seen);
        let (stdout, stdout_truncated) = state.stdout.render(stdout_limit);
        let (stderr, stderr_truncated) = state.stderr.render(stderr_limit);
        Ok(CapturedOutput {
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

#[async_trait]
impl ToolOutputSink for CapturingSink {
    async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ToolError::Output("capture lock was poisoned".to_owned()))?;
            match chunk.stream {
                ToolOutputStream::Stdout => state.stdout.push(chunk.content.as_bytes()),
                ToolOutputStream::Stderr => state.stderr.push(chunk.content.as_bytes()),
            }
        }
        self.upstream.emit(chunk).await
    }
}

struct CapturedOutput {
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

struct TailBuffer {
    bytes: Vec<u8>,
    cap: usize,
    total_seen: usize,
}

impl TailBuffer {
    fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            cap,
            total_seen: 0,
        }
    }

    fn push(&mut self, incoming: &[u8]) {
        self.total_seen = self.total_seen.saturating_add(incoming.len());
        if self.cap == 0 {
            return;
        }
        if incoming.len() >= self.cap {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&incoming[incoming.len() - self.cap..]);
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(incoming.len())
            .saturating_sub(self.cap);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
        self.bytes.extend_from_slice(incoming);
    }

    fn render(&self, limit: usize) -> (String, bool) {
        if self.total_seen <= limit {
            return (String::from_utf8_lossy(&self.bytes).into_owned(), false);
        }
        if limit == 0 {
            return (String::new(), true);
        }
        let provisional_dropped = self.total_seen.saturating_sub(limit);
        let provisional_marker = format!("[truncated {provisional_dropped} bytes; showing tail]\n");
        if provisional_marker.len() >= limit {
            let start = self.bytes.len().saturating_sub(limit);
            return (
                String::from_utf8_lossy(&self.bytes[start..]).into_owned(),
                true,
            );
        }
        let tail_limit = limit - provisional_marker.len();
        let retained = self.bytes.len().min(tail_limit);
        let dropped = self.total_seen.saturating_sub(retained);
        let marker = format!("[truncated {dropped} bytes; showing tail]\n");
        if marker.len() >= limit {
            let start = self.bytes.len().saturating_sub(limit);
            return (
                String::from_utf8_lossy(&self.bytes[start..]).into_owned(),
                true,
            );
        }
        let adjusted_tail_limit = limit.saturating_sub(marker.len());
        let start = self.bytes.len().saturating_sub(adjusted_tail_limit);
        (
            format!("{marker}{}", String::from_utf8_lossy(&self.bytes[start..])),
            true,
        )
    }
}

fn allocate_stream_limits(total: usize, stdout: usize, stderr: usize) -> (usize, usize) {
    if stdout == 0 {
        return (0, total);
    }
    if stderr == 0 {
        return (total, 0);
    }
    let combined = stdout.saturating_add(stderr).max(1);
    let stdout_limit = total.saturating_mul(stdout) / combined;
    (stdout_limit, total.saturating_sub(stdout_limit))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::Mutex;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    struct StreamingExecutor;

    struct SecretRedactor;

    impl CommandFixtureRedactor for SecretRedactor {
        fn redact(&self, value: &str) -> String {
            value.replace("secret-canary", "[REDACTED]")
        }
    }

    #[async_trait]
    impl CommandExecutor for StreamingExecutor {
        async fn run(
            &self,
            _request: CommandRequest,
            _cancellation: CancellationToken,
            output: Arc<dyn ToolOutputSink>,
        ) -> Result<CommandOutcome, ToolError> {
            output
                .emit(ToolOutputChunk {
                    stream: ToolOutputStream::Stdout,
                    content: "0123456789".to_owned(),
                })
                .await?;
            output
                .emit(ToolOutputChunk {
                    stream: ToolOutputStream::Stderr,
                    content: "warning".to_owned(),
                })
                .await?;
            Ok(CommandOutcome { exit_code: 7 })
        }
    }

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<ToolOutputChunk>>);

    #[async_trait]
    impl ToolOutputSink for RecordingSink {
        async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
            self.0
                .lock()
                .map_err(|_| ToolError::Output("test lock".to_owned()))?
                .push(chunk);
            Ok(())
        }
    }

    #[tokio::test]
    async fn streams_full_output_but_returns_a_tail_biased_cap() {
        let root = tempdir().expect("temp directory");
        let sink = Arc::new(RecordingSink::default());
        let context = ToolContext::new(root.path())
            .expect("context")
            .with_output(sink.clone());
        let tool = BashTool::new(
            Arc::new(StreamingExecutor),
            ToolLimits {
                max_result_bytes: 8,
                ..ToolLimits::default()
            },
        );
        let result = tool
            .execute(&context, json!({"command": "ignored"}))
            .await
            .expect("command result");
        assert_eq!(result.data["exit_code"], 7);
        assert!(result.truncated);
        assert!(result.content.contains("89"));
        assert_eq!(sink.0.lock().expect("recording").len(), 2);
    }

    #[tokio::test]
    async fn command_recording_replays_exact_relative_occurrence_without_running_an_executor() {
        let record_root = tempdir().expect("record workspace");
        let replay_root = tempdir().expect("replay workspace");
        let fixtures = tempdir().expect("fixtures");
        let dangerous_command = "nc 127.0.0.1 9 < secret";
        let recorder = RecordingCommandExecutor::new(
            Arc::new(StreamingExecutor),
            fixtures.path(),
            record_root.path(),
        )
        .expect("recorder");
        let recorded_sink = Arc::new(RecordingSink::default());
        let expected_outcome = recorder
            .run(
                CommandRequest {
                    command: dangerous_command.to_owned(),
                    cwd: record_root.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                recorded_sink.clone(),
            )
            .await
            .expect("record command");

        let offline_executor = ReplayCommandExecutor::load(fixtures.path(), replay_root.path())
            .expect("replay executor");
        let replayed_sink = Arc::new(RecordingSink::default());
        let actual_outcome = offline_executor
            .run(
                CommandRequest {
                    command: dangerous_command.to_owned(),
                    cwd: replay_root.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                replayed_sink.clone(),
            )
            .await
            .expect("replay command");

        assert_eq!(actual_outcome, expected_outcome);
        assert_eq!(
            replayed_sink.0.lock().expect("replayed output").as_slice(),
            recorded_sink.0.lock().expect("recorded output").as_slice()
        );
        assert!(matches!(
            offline_executor
                .run(
                    CommandRequest {
                        command: dangerous_command.to_owned(),
                        cwd: replay_root.path().to_path_buf(),
                        env: BTreeMap::new(),
                    },
                    CancellationToken::default(),
                    Arc::new(RecordingSink::default()),
                )
                .await,
            Err(ToolError::Command(message)) if message.contains("exhausted")
        ));
    }

    #[tokio::test]
    async fn command_fixture_redactor_runs_before_any_fixture_bytes_reach_disk() {
        let workspace = tempdir().expect("workspace");
        let fixtures = tempdir().expect("fixtures");
        let recorder = RecordingCommandExecutor::new_with_redactor(
            Arc::new(StreamingExecutor),
            fixtures.path(),
            workspace.path(),
            Arc::new(SecretRedactor),
        )
        .expect("recorder");
        recorder
            .run(
                CommandRequest {
                    command: "printf secret-canary".to_owned(),
                    cwd: workspace.path().to_path_buf(),
                    env: BTreeMap::from([("TOKEN".to_owned(), "secret-canary".to_owned())]),
                },
                CancellationToken::default(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("record command");
        let fixture =
            std::fs::read_to_string(fixtures.path().join(COMMAND_REPLAY_FILE)).expect("fixture");
        assert!(!fixture.contains("secret-canary"));
        assert!(fixture.contains("[REDACTED]"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(fixtures.path().join(COMMAND_REPLAY_FILE))
                .expect("fixture metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replay_recovers_a_complete_private_stale_temp_and_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let record_workspace = tempdir().expect("record workspace");
        let replay_workspace = tempdir().expect("replay workspace");
        let fixtures = tempdir().expect("fixtures");
        let recorder = RecordingCommandExecutor::new(
            Arc::new(StreamingExecutor),
            fixtures.path(),
            record_workspace.path(),
        )
        .expect("recorder");
        recorder
            .run(
                CommandRequest {
                    command: "printf recovered".to_owned(),
                    cwd: record_workspace.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("record command");
        let installed = fixtures.path().join(COMMAND_REPLAY_FILE);
        let temporary = fixtures.path().join(COMMAND_REPLAY_TEMP_FILE);
        std::fs::rename(&installed, &temporary).expect("simulate pre-rename crash");
        let replay = ReplayCommandExecutor::load(fixtures.path(), replay_workspace.path())
            .expect("recover stale temp");
        assert!(installed.is_file());
        assert!(!temporary.exists());
        replay
            .run(
                CommandRequest {
                    command: "printf recovered".to_owned(),
                    cwd: replay_workspace.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("replay recovered occurrence");

        std::fs::remove_file(&installed).expect("remove fixture");
        let target = fixtures.path().join("attacker.json");
        std::fs::write(&target, b"[]").expect("attacker file");
        symlink(&target, &installed).expect("fixture symlink");
        assert!(matches!(
            ReplayCommandExecutor::load(fixtures.path(), replay_workspace.path()),
            Err(ToolError::Command(message)) if message.contains("regular file")
        ));
    }

    struct BlockingExecutor;

    #[async_trait]
    impl CommandExecutor for BlockingExecutor {
        async fn run(
            &self,
            _request: CommandRequest,
            cancellation: CancellationToken,
            _output: Arc<dyn ToolOutputSink>,
        ) -> Result<CommandOutcome, ToolError> {
            cancellation.cancelled().await;
            Err(ToolError::Cancelled)
        }
    }

    #[tokio::test]
    async fn injected_commands_observe_cancellation() {
        let root = tempdir().expect("temp directory");
        let cancellation = CancellationToken::default();
        let context = ToolContext::new(root.path())
            .expect("context")
            .with_cancellation(cancellation.clone());
        let tool = BashTool::new(Arc::new(BlockingExecutor), ToolLimits::default());
        let task =
            tokio::spawn(
                async move { tool.execute(&context, json!({"command": "ignored"})).await },
            );
        tokio::task::yield_now().await;
        cancellation.cancel();
        assert!(matches!(
            task.await.expect("join"),
            Err(ToolError::Cancelled)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn hung_output_readers_are_aborted_after_a_bounded_drain() {
        let reader = tokio::spawn(async { std::future::pending::<Result<(), ToolError>>().await });
        let drain = tokio::spawn(finish_output_task(reader));
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(drain.await.expect("drain join").is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_cancellation_kills_a_descendant_holding_the_pipes() {
        let root = tempdir().expect("temp directory");
        let cancellation = CancellationToken::default();
        let run_cancellation = cancellation.clone();
        let executor = TokioCommandExecutor::default();
        let run = tokio::spawn(async move {
            executor
                .run(
                    CommandRequest {
                        command: "sleep 30 & wait".to_owned(),
                        cwd: root.path().to_path_buf(),
                        env: BTreeMap::new(),
                    },
                    run_cancellation,
                    Arc::new(crate::NoopOutputSink),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(3), run)
            .await
            .expect("bounded cancellation")
            .expect("executor join");
        assert!(matches!(outcome, Err(ToolError::Cancelled)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_executor_disarms_and_reaps_watchdog_on_normal_completion() {
        let root = tempdir().expect("temp directory");
        let sink = Arc::new(RecordingSink::default());
        let outcome = TokioCommandExecutor::default()
            .run(
                CommandRequest {
                    command: "printf normal".to_owned(),
                    cwd: root.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                sink.clone(),
            )
            .await
            .expect("normal command");
        assert_eq!(outcome.exit_code, 0);
        assert!(
            sink.0
                .lock()
                .expect("recording")
                .iter()
                .any(|chunk| chunk.content.contains("normal"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn executor_waits_for_background_group_members_before_returning() {
        let root = tempdir().expect("temp directory");
        let pid_file = root.path().join("background.pid");
        let outcome = TokioCommandExecutor::default()
            .run(
                CommandRequest {
                    command: "sleep 30 & printf '%s\\n' \"$!\" > background.pid".to_owned(),
                    cwd: root.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                Arc::new(crate::NoopOutputSink),
            )
            .await
            .expect("command outcome");
        assert_eq!(outcome.exit_code, 0);
        let background = read_test_pid(&pid_file).await;
        assert!(
            rustix::process::test_kill_process(background).is_err(),
            "background same-group process survived executor return"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lease_descriptor_is_not_inherited_by_user_or_unrelated_commands() {
        let root = tempdir().expect("temp directory");
        let lease = Arc::new(
            ExecutionLease::acquire(root.path().join("execution.lock")).expect("execution lease"),
        );
        let descriptor = lease.test_watchdog_raw_fd().to_string();
        let probe = "if eval \"true <&$1\" 2>/dev/null; then exit 90; else exit 0; fi";
        let unrelated = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(probe)
            .arg("lease-probe")
            .arg(&descriptor)
            .status()
            .expect("unrelated descriptor probe");
        assert!(unrelated.success(), "unrelated child inherited lease fd");

        let user_probe =
            format!("if eval \"true <&{descriptor}\" 2>/dev/null; then exit 90; else exit 0; fi");
        let outcome = TokioCommandExecutor::with_execution_lease(lease)
            .run(
                CommandRequest {
                    command: user_probe,
                    cwd: root.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                Arc::new(crate::NoopOutputSink),
            )
            .await
            .expect("user command descriptor probe");
        assert_eq!(outcome.exit_code, 0, "user command inherited lease fd");
    }

    #[cfg(unix)]
    #[test]
    fn watchdog_subprocess_helper() {
        if std::env::var_os("ROTTWEILER_WATCHDOG_HELPER").is_none() {
            return;
        }
        let ready = std::env::var("ROTTWEILER_WATCHDOG_READY").expect("ready path");
        let sentinel = std::env::var("ROTTWEILER_WATCHDOG_SENTINEL").expect("sentinel path");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("helper runtime");
        let executor = match std::env::var_os("ROTTWEILER_WATCHDOG_LEASE") {
            Some(path) => TokioCommandExecutor::with_execution_lease(Arc::new(
                ExecutionLease::acquire(path).expect("helper execution lease"),
            )),
            None => TokioCommandExecutor::default(),
        };
        runtime
            .block_on(executor.run(
                CommandRequest {
                    command: "printf '%s\\n' \"$$\" > \"$ROTTWEILER_WATCHDOG_READY\"; sleep 2; printf survived > \"$ROTTWEILER_WATCHDOG_SENTINEL\"; sleep 30".to_owned(),
                    cwd: std::env::temp_dir(),
                    env: BTreeMap::from([
                        ("ROTTWEILER_WATCHDOG_READY".to_owned(), ready),
                        ("ROTTWEILER_WATCHDOG_SENTINEL".to_owned(), sentinel),
                    ]),
                },
                CancellationToken::default(),
                Arc::new(crate::NoopOutputSink),
            ))
            .expect("helper command");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigkill_of_executor_parent_kills_group_and_prevents_delayed_side_effects() {
        let root = tempdir().expect("temp directory");
        let ready = root.path().join("command.pid");
        let watchdog_pid_file = root.path().join("watchdog.pid");
        let sentinel = root.path().join("sentinel");
        let executable = std::env::current_exe().expect("current test executable");
        let mut helper_command = Command::new(executable);
        helper_command
            .arg("--exact")
            .arg("bash::tests::watchdog_subprocess_helper")
            .arg("--nocapture")
            .env("ROTTWEILER_WATCHDOG_HELPER", "1")
            .env("ROTTWEILER_WATCHDOG_READY", &ready)
            .env("ROTTWEILER_WATCHDOG_SENTINEL", &sentinel)
            .env("ROTTWEILER_WATCHDOG_TEST_PID_FILE", &watchdog_pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut helper = helper_command.spawn().expect("spawn helper");
        wait_for_test_file(&mut helper, &ready).await;
        wait_for_test_file(&mut helper, &watchdog_pid_file).await;
        let command_pid = read_test_pid(&ready).await;
        let watchdog_pid = read_test_pid(&watchdog_pid_file).await;
        let helper_pid = helper
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
            .expect("helper pid");
        rustix::process::kill_process(helper_pid, rustix::process::Signal::KILL)
            .expect("kill helper");
        tokio::time::timeout(Duration::from_secs(3), helper.wait())
            .await
            .expect("helper exit timeout")
            .expect("helper wait");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let group_gone = rustix::process::test_kill_process_group(command_pid).is_err();
            let watchdog_gone = rustix::process::test_kill_process(watchdog_pid).is_err();
            if group_gone && watchdog_gone {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "orphan process survived"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::sleep(Duration::from_millis(2100)).await;
        assert!(!sentinel.exists(), "orphan command wrote delayed sentinel");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_lease_blocks_resumer_until_killed_group_is_absent() {
        let root = tempdir().expect("temp directory");
        let lease_path = root.path().join("execution.lock");
        let pause = root.path().join("pause-watchdog");
        std::fs::write(&pause, b"pause").expect("watchdog pause marker");
        let ready = root.path().join("command.pid");
        let watchdog_pid_file = root.path().join("watchdog.pid");
        let sentinel = root.path().join("sentinel");
        let executable = std::env::current_exe().expect("current test executable");
        let mut helper_command = Command::new(executable);
        helper_command
            .arg("--exact")
            .arg("bash::tests::watchdog_subprocess_helper")
            .arg("--nocapture")
            .env("ROTTWEILER_WATCHDOG_HELPER", "1")
            .env("ROTTWEILER_WATCHDOG_READY", &ready)
            .env("ROTTWEILER_WATCHDOG_SENTINEL", &sentinel)
            .env("ROTTWEILER_WATCHDOG_TEST_PID_FILE", &watchdog_pid_file)
            .env("ROTTWEILER_WATCHDOG_PAUSE_FILE", &pause)
            .env("ROTTWEILER_WATCHDOG_LEASE", &lease_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut helper = helper_command.spawn().expect("spawn helper");
        wait_for_test_file(&mut helper, &ready).await;
        wait_for_test_file(&mut helper, &watchdog_pid_file).await;
        let command_pid = read_test_pid(&ready).await;
        let watchdog_pid = read_test_pid(&watchdog_pid_file).await;
        let helper_pid = helper
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
            .expect("helper pid");
        rustix::process::kill_process(helper_pid, rustix::process::Signal::KILL)
            .expect("kill helper");
        helper.wait().await.expect("helper wait");

        let (acquired_tx, mut acquired_rx) = tokio::sync::mpsc::unbounded_channel();
        let resumer_path = lease_path.clone();
        let resumer = tokio::task::spawn_blocking(move || {
            let lease = ExecutionLease::acquire(resumer_path).expect("resumer lease");
            acquired_tx.send(lease).expect("report acquired lease");
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), acquired_rx.recv())
                .await
                .is_err(),
            "resumer acquired while watchdog was deliberately paused"
        );
        assert!(
            rustix::process::test_kill_process_group(command_pid).is_ok(),
            "paused watchdog killed command group too early"
        );

        std::fs::remove_file(&pause).expect("release watchdog");
        let resumed_lease = tokio::time::timeout(Duration::from_secs(3), acquired_rx.recv())
            .await
            .expect("resumer barrier timeout")
            .expect("resumer lease channel");
        assert!(
            rustix::process::test_kill_process_group(command_pid).is_err(),
            "lease released before command group disappearance"
        );
        assert!(
            rustix::process::test_kill_process(watchdog_pid).is_err(),
            "lease released before watchdog exit"
        );
        drop(resumed_lease);
        resumer.await.expect("resumer task");
        assert!(!sentinel.exists(), "orphan command wrote delayed sentinel");
    }

    #[cfg(unix)]
    async fn wait_for_test_file(helper: &mut Child, path: &std::path::Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !path.exists() {
            assert!(
                helper.try_wait().expect("helper status").is_none(),
                "helper exited before {} was created",
                path.display()
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "helper readiness timeout"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    async fn read_test_pid(path: &std::path::Path) -> rustix::process::Pid {
        let raw = tokio::fs::read_to_string(path)
            .await
            .expect("pid file")
            .trim()
            .parse::<i32>()
            .expect("numeric pid");
        rustix::process::Pid::from_raw(raw).expect("positive pid")
    }

    #[test]
    fn bash_declares_all_ambient_capabilities() {
        let descriptor =
            BashTool::new(Arc::new(StreamingExecutor), ToolLimits::default()).descriptor();
        for capability in [
            ToolCapability::ReadFilesystem,
            ToolCapability::WriteFilesystem,
            ToolCapability::Network,
            ToolCapability::Execute,
        ] {
            assert!(descriptor.capabilities.contains(&capability));
        }
    }
}
