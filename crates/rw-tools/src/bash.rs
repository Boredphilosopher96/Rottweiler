use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use rw_sandbox::{
    EgressPolicy, NetworkPolicy as SandboxNetworkPolicy, SandboxPolicy, SupervisedEgressProxy,
    UpstreamProxy, normalize_egress_domain, shell_launch_plan,
};
use rw_types::{ToolCapability, ToolOutputStream};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::{Duration, sleep};

use crate::BackgroundProcessManager;
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
    /// Public domains requested in addition to the default package registries.
    /// Permission approvals bind to this exact normalized list.
    #[serde(default)]
    pub network_domains: Vec<String>,
    /// Selects the native OS sandbox boundary. Unsandboxed execution is an
    /// explicit escape hatch and is always permission-gated by the engine.
    #[serde(default)]
    pub sandbox: BashSandboxMode,
    /// Return immediately while the session process manager supervises the
    /// command. Output is retrieved with `background_output`.
    #[serde(default)]
    pub run_in_background: bool,
}

fn default_cwd() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest {
    pub command: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub network_domains: Vec<String>,
    pub sandbox: BashSandboxMode,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BashSandboxMode {
    #[default]
    Sandboxed,
    Unsandboxed,
    /// Internal write-denied sandbox used only after `run_in_background` has
    /// passed validation.
    ReadOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandOutcome {
    pub exit_code: i32,
}

/// Conservative built-in safe-list result used by the permission chokepoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandSafety {
    /// The entire command is a recognized read-only operation and may run
    /// without a prompt, but still inside the OS sandbox.
    SafeListed,
    /// The command is unknown, compound, interpolated, or potentially
    /// mutating.  Normal permission policy applies.
    RequiresApproval,
}

/// One immutable command classifier shared by permission policy and execution.
/// User patterns are accepted only from the already-filtered user config layer.
#[derive(Clone, Debug, Default)]
pub struct CommandSafetyClassifier {
    user_patterns: Vec<GlobMatcher>,
}

impl CommandSafetyClassifier {
    /// Compiles user-scoped command globs. Invalid patterns fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured glob cannot be compiled.
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        let user_patterns = patterns
            .iter()
            .map(|pattern| {
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build()
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| {
                        format!("invalid sandbox safe-list pattern {pattern:?}: {error}")
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { user_patterns })
    }

    #[must_use]
    pub fn classify(&self, command: &str) -> CommandSafety {
        let Some(segments) = safe_command_segments(command) else {
            return CommandSafety::RequiresApproval;
        };
        if segments.iter().all(|(segment, _)| {
            built_in_safe_segment(segment)
                || self
                    .user_patterns
                    .iter()
                    .any(|pattern| pattern.is_match(segment))
        }) {
            CommandSafety::SafeListed
        } else {
            CommandSafety::RequiresApproval
        }
    }
}

/// Classifies a canonical shell command for the built-in no-prompt safe-list.
///
/// This list is intentionally small.  Shell interpolation and control syntax
/// are rejected before tokenization, and only the real `git status` built-in
/// (with ordinary option/path arguments) is accepted.  A user may extend the
/// safe-list through user-scoped permission configuration; project content
/// never calls this function with additional rules.
#[must_use]
pub fn classify_safe_command(command: &str) -> CommandSafety {
    CommandSafetyClassifier::default().classify(command)
}

fn built_in_safe_segment(command: &str) -> bool {
    let Ok(argv) = shell_words::split(command) else {
        return false;
    };
    if audited_system_git().is_none() || argv.first().map(String::as_str) != Some("git") {
        return false;
    }
    argv.get(1).is_some_and(|argument| argument == "status")
        && safe_git_status_arguments(&argv[2..])
}

fn safe_command_segments(command: &str) -> Option<Vec<(String, Option<String>)>> {
    if command.is_empty()
        || command.contains(['\n', '\r', '`', '$'])
        || command.as_bytes().contains(&0)
    {
        return None;
    }
    let chars = command.char_indices().collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let (offset, character) = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        match character {
            '\\' if !single => {
                escaped = true;
                index += 1;
                continue;
            }
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            _ => {}
        }
        if !single && !double {
            let next = chars.get(index + 1).map(|(_, next)| *next);
            let delimiter = match (character, next) {
                ('&', Some('&')) => Some((2, "&&")),
                ('|', Some('|')) => Some((2, "||")),
                (';', _) => Some((1, ";")),
                ('|' | '&' | '<' | '>' | '(' | ')', _) => return None,
                _ => None,
            };
            if let Some((delimiter_len, operator)) = delimiter {
                let segment = command.get(start..offset)?.trim();
                let canonical = shell_words::split(segment).ok()?.join(" ");
                if canonical.is_empty() {
                    return None;
                }
                segments.push((canonical, Some(operator.to_owned())));
                index += delimiter_len;
                start = chars.get(index).map_or(command.len(), |(next, _)| *next);
                continue;
            }
        }
        index += 1;
    }
    if single || double || escaped {
        return None;
    }
    let canonical = shell_words::split(command.get(start..)?.trim())
        .ok()?
        .join(" ");
    if canonical.is_empty() {
        return None;
    }
    segments.push((canonical, None));
    Some(segments)
}

fn safe_git_status_arguments(arguments: &[String]) -> bool {
    let mut pathspecs = false;
    for argument in arguments {
        if pathspecs {
            continue;
        }
        if argument == "--" {
            pathspecs = true;
            continue;
        }
        if !matches!(
            argument.as_str(),
            "--short"
                | "-s"
                | "--branch"
                | "-b"
                | "--show-stash"
                | "--porcelain"
                | "--porcelain=v1"
                | "--porcelain=v2"
                | "--untracked-files=no"
                | "--untracked-files=normal"
                | "--untracked-files=all"
                | "-uno"
                | "-unormal"
                | "-uall"
                | "--ignored=no"
                | "--ignored=matching"
                | "--ignored=traditional"
                | "--renames"
                | "--no-renames"
                | "--ahead-behind"
                | "--no-ahead-behind"
        ) {
            return false;
        }
    }
    true
}

pub(crate) fn audited_system_git() -> Option<&'static PathBuf> {
    static SYSTEM_GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    SYSTEM_GIT.get_or_init(resolve_audited_system_git).as_ref()
}

fn resolve_audited_system_git() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        for candidate in [Path::new("/usr/bin/git"), Path::new("/bin/git")] {
            let Ok(canonical) = candidate.canonicalize() else {
                continue;
            };
            if !canonical.starts_with("/usr/bin") && !canonical.starts_with("/bin") {
                continue;
            }
            let Ok(metadata) = canonical.metadata() else {
                continue;
            };
            if metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0 {
                return Some(canonical);
            }
        }
    }
    None
}

/// Injected process boundary. Core must approve the bash manifest before this is called.
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    /// Whether this executor can safely supervise a command after the
    /// initiating tool call returns.
    fn supports_background(&self) -> bool {
        false
    }

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
        acquire_execution_lease(path.as_ref(), true)
    }

    /// Opens an execution lease without waiting behind another process.
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` through [`ToolError::Io`] when another process
    /// already owns the workspace lease, in addition to the safety errors
    /// documented by [`Self::acquire`].
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Self, ToolError> {
        acquire_execution_lease(path.as_ref(), false)
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
fn acquire_execution_lease(path: &Path, wait: bool) -> Result<ExecutionLease, ToolError> {
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
    lock_execution_lease(&file, path, wait)?;
    rustix::fs::fsync(&parent)
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "synchronize execution lease directory",
            path: parent_path.to_path_buf(),
            source,
        })?;
    Ok(ExecutionLease { file })
}

#[cfg(unix)]
fn lock_execution_lease(file: &std::fs::File, path: &Path, wait: bool) -> Result<(), ToolError> {
    let operation = if wait {
        rustix::fs::FlockOperation::LockExclusive
    } else {
        rustix::fs::FlockOperation::NonBlockingLockExclusive
    };
    loop {
        match rustix::fs::flock(file, operation) {
            Ok(()) => return Ok(()),
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
}

#[cfg(not(unix))]
fn acquire_execution_lease(path: &Path, _wait: bool) -> Result<ExecutionLease, ToolError> {
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

const COMMAND_REPLAY_FILE: &str = "commands.json";
const COMMAND_REPLAY_TEMP_FILE: &str = "commands.json.tmp";
const COMMAND_REPLAY_ROOT: &str = "${ROTTWEILER_COMMAND_ROOT}";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CanonicalCommandRequest {
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
    sandbox: Option<Arc<SandboxPolicy>>,
    policy_egress_available: bool,
    upstream_proxy: Option<UpstreamProxy>,
    safety: Arc<CommandSafetyClassifier>,
    #[cfg(test)]
    proxy_lifecycles: Option<Arc<Mutex<Vec<rw_sandbox::ProxyLifecycle>>>>,
    #[cfg(all(test, unix))]
    launch_gate_hook: Option<Arc<LaunchGateTestHook>>,
}

#[cfg(all(test, unix))]
#[derive(Debug, Default)]
struct LaunchGateTestHook {
    child_id: std::sync::atomic::AtomicU32,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(all(test, unix))]
impl LaunchGateTestHook {
    async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    fn release(&self) {
        self.release.notify_one();
    }

    fn child_id(&self) -> Option<rustix::process::Pid> {
        let raw_pid =
            i32::try_from(self.child_id.load(std::sync::atomic::Ordering::Acquire)).ok()?;
        rustix::process::Pid::from_raw(raw_pid)
    }
}

impl TokioCommandExecutor {
    /// Retains the session execution lease for this process boundary.
    #[must_use]
    pub fn with_execution_lease(execution_lease: Arc<ExecutionLease>) -> Self {
        Self {
            execution_lease: Some(execution_lease),
            sandbox: None,
            policy_egress_available: false,
            upstream_proxy: None,
            safety: Arc::new(CommandSafetyClassifier::default()),
            #[cfg(test)]
            proxy_lifecycles: None,
            #[cfg(all(test, unix))]
            launch_gate_hook: None,
        }
    }

    /// Runs every command inside the supplied native OS sandbox.
    #[must_use]
    pub fn sandboxed(mut self, policy: Arc<SandboxPolicy>) -> Self {
        self.sandbox = Some(policy);
        self
    }

    /// Uses the exact classifier shared with the permission gate and bash tool.
    #[must_use]
    pub fn with_command_safety(mut self, safety: Arc<CommandSafetyClassifier>) -> Self {
        self.safety = safety;
        self
    }

    /// Enables per-command supervised policy proxies on a backend that can bind
    /// the child to their exact endpoint.
    #[must_use]
    pub const fn with_policy_egress(mut self, available: bool) -> Self {
        self.policy_egress_available = available;
        self
    }

    /// Chains every approved command proxy through an explicit corporate
    /// proxy after the local target policy has allowed the destination.
    #[must_use]
    pub fn with_upstream_proxy(mut self, proxy: Option<UpstreamProxy>) -> Self {
        self.upstream_proxy = proxy;
        self
    }

    #[cfg(all(test, target_os = "macos"))]
    fn with_proxy_lifecycle_observer(
        mut self,
        lifecycles: Arc<Mutex<Vec<rw_sandbox::ProxyLifecycle>>>,
    ) -> Self {
        self.proxy_lifecycles = Some(lifecycles);
        self
    }

    #[cfg(all(test, unix))]
    fn with_launch_gate_hook(mut self, hook: Arc<LaunchGateTestHook>) -> Self {
        self.launch_gate_hook = Some(hook);
        self
    }
}

#[async_trait]
impl CommandExecutor for TokioCommandExecutor {
    fn supports_background(&self) -> bool {
        self.sandbox.is_some()
    }

    #[allow(clippy::too_many_lines)]
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
        let safe = request.sandbox != BashSandboxMode::Unsandboxed
            && request.network_domains.is_empty()
            && self.safety.classify(&request.command) == CommandSafety::SafeListed;
        let egress_proxy = command_egress_proxy(
            &request,
            safe,
            self.policy_egress_available,
            self.upstream_proxy.as_ref(),
        )?;
        #[cfg(test)]
        if let (Some(proxy), Some(lifecycles)) =
            (egress_proxy.as_ref(), self.proxy_lifecycles.as_ref())
        {
            lifecycles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(proxy.lifecycle());
        }
        let read_only_policy = (request.sandbox == BashSandboxMode::ReadOnly)
            .then(|| self.sandbox.as_deref().map(SandboxPolicy::read_only))
            .flatten();
        let sandbox = if request.sandbox == BashSandboxMode::ReadOnly {
            read_only_policy.as_ref()
        } else if request.sandbox == BashSandboxMode::Sandboxed {
            self.sandbox.as_deref()
        } else {
            None
        };
        let mut guarded = guarded_process(&request, sandbox, egress_proxy.as_ref())?;
        let mut child = guarded
            .command
            .spawn()
            .map_err(|error| ToolError::Command(error.to_string()))?;
        #[cfg(target_os = "linux")]
        drop(guarded.helper_pin.take());
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
        #[cfg(all(test, unix))]
        if let Some(hook) = &self.launch_gate_hook {
            hook.child_id.store(
                child_id.unwrap_or_default(),
                std::sync::atomic::Ordering::Release,
            );
            hook.reached.notify_one();
            hook.release.notified().await;
        }
        let launch_result = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                terminate_process_group(child_id);
                let _ = child.start_kill();
                let _ = child.wait().await;
                terminate_and_wait_process_group(child_id).await?;
                watchdog.disarm().await?;
                return Err(ToolError::Cancelled);
            }
            result = launch_gate.write_all(b"armed\n") => result,
        };
        if let Err(error) = launch_result {
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
            biased;
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
            status = child.wait() => status,
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

fn command_egress_proxy(
    request: &CommandRequest,
    safe: bool,
    available: bool,
    upstream_proxy: Option<&UpstreamProxy>,
) -> Result<Option<SupervisedEgressProxy>, ToolError> {
    if request.network_domains.is_empty() {
        return Ok(None);
    }
    if safe || !available {
        return Err(ToolError::Command(
            "requested command domains cannot be routed safely on this host".to_owned(),
        ));
    }
    let mut policy = EgressPolicy::default();
    for domain in &request.network_domains {
        if !policy.allow_domain(domain) {
            return Err(ToolError::InvalidInput(format!(
                "invalid requested network domain {domain:?}"
            )));
        }
    }
    SupervisedEgressProxy::start_with_upstream(policy, upstream_proxy.cloned())
        .map(Some)
        .map_err(|error| {
            ToolError::Command(format!("supervised egress proxy could not start: {error}"))
        })
}

fn guarded_process(
    request: &CommandRequest,
    sandbox: Option<&SandboxPolicy>,
    egress_proxy: Option<&SupervisedEgressProxy>,
) -> Result<GuardedCommand, ToolError> {
    #[cfg(target_os = "macos")]
    if sandbox.is_some() && command_can_escape_process_group(&request.command) {
        return Err(ToolError::Command(
            "daemonizing commands are unavailable until descendant lifetime isolation is active"
                .to_owned(),
        ));
    }
    let safe_git = safe_git_invocation(&request.command);
    let hardened_git_compound = hardened_git_compound(&request.command);
    let network = egress_proxy.is_some() && safe_git.is_none() && hardened_git_compound.is_none();
    let shell_command = hardened_git_compound.as_deref().unwrap_or(&request.command);
    let shell_args = safe_git.as_ref().map_or_else(
        || {
            vec![
                OsString::from("-c"),
                OsString::from("IFS= read -r _ || exit 125; exec /bin/sh -c \"$1\""),
                OsString::from("rottweiler-command-launcher"),
                OsString::from(shell_command),
            ]
        },
        |argv| {
            let mut shell_args = vec![
                OsString::from("-c"),
                OsString::from("IFS= read -r _ || exit 125; exec \"$@\""),
                OsString::from("rottweiler-safe-git-launcher"),
            ];
            shell_args.extend(argv.iter().map(OsString::from));
            shell_args
        },
    );
    #[cfg(target_os = "linux")]
    let mut helper_pin = None;
    let (program, args) = if let Some(base_policy) = sandbox {
        let policy = if network {
            let proxy = egress_proxy.ok_or_else(|| {
                ToolError::Command(
                    "network was approved but the supervised egress proxy is unavailable"
                        .to_owned(),
                )
            })?;
            base_policy.with_network(SandboxNetworkPolicy::PolicyProxy {
                port: proxy.address().port(),
                relay_path: proxy.relay_path().map(Path::to_path_buf),
            })
        } else {
            base_policy.with_network(SandboxNetworkPolicy::Deny)
        };
        let executable = std::env::current_exe()
            .map_err(|error| ToolError::Command(format!("sandbox helper unavailable: {error}")))?;
        let plan = shell_launch_plan(&policy, &executable, Path::new("/bin/sh"), &shell_args)
            .map_err(|error| ToolError::Command(error.to_string()))?;
        #[cfg(target_os = "linux")]
        let plan = {
            let mut plan = plan;
            helper_pin = plan.take_helper_pin();
            plan
        };
        (plan.program, plan.args)
    } else {
        (PathBuf::from("/bin/sh"), shell_args)
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&request.cwd)
        .envs(&request.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    sanitize_shell_control_environment(&mut command);
    configure_proxy_environment(&mut command, network.then_some(egress_proxy).flatten());
    if safe_git.is_some() || hardened_git_compound.is_some() {
        sanitize_git_environment(&mut command, request);
    }
    #[cfg(unix)]
    command.process_group(0);
    Ok(GuardedCommand {
        command,
        #[cfg(target_os = "linux")]
        helper_pin,
    })
}

struct GuardedCommand {
    command: Command,
    #[cfg(target_os = "linux")]
    helper_pin: Option<std::fs::File>,
}

#[cfg(target_os = "macos")]
fn command_can_escape_process_group(command: &str) -> bool {
    let Ok(words) = shell_words::split(command) else {
        return true;
    };
    words.iter().any(|word| {
        Path::new(word)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "setsid" | "nohup" | "daemon"))
    })
}

fn sanitize_shell_control_environment(command: &mut Command) {
    for key in ["BASH_ENV", "ENV", "SHELLOPTS", "CDPATH"] {
        command.env_remove(key);
    }
}

fn safe_git_invocation(command: &str) -> Option<Vec<String>> {
    let segments = safe_command_segments(command)?;
    if segments.len() != 1 || !built_in_safe_segment(&segments[0].0) {
        return None;
    }
    hardened_git_argv(&segments[0].0)
}

fn hardened_git_compound(command: &str) -> Option<String> {
    let segments = safe_command_segments(command)?;
    if segments.len() < 2
        || !segments
            .iter()
            .all(|(segment, _)| built_in_safe_segment(segment))
    {
        return None;
    }
    let mut hardened = String::new();
    for (segment, operator) in segments {
        let argv = hardened_git_argv(&segment)?;
        if !hardened.is_empty() {
            hardened.push(' ');
        }
        hardened.push_str(
            &argv
                .iter()
                .map(|argument| shell_words::quote(argument).into_owned())
                .collect::<Vec<_>>()
                .join(" "),
        );
        if let Some(operator) = operator {
            hardened.push(' ');
            hardened.push_str(&operator);
        }
    }
    Some(hardened)
}

fn hardened_git_argv(command: &str) -> Option<Vec<String>> {
    let mut supplied = shell_words::split(command).ok()?;
    let git = audited_system_git()?;
    supplied.remove(0);
    let status_arguments = supplied.split_off(1);
    let mut argv = vec![
        git.to_string_lossy().into_owned(),
        "-c".to_owned(),
        "core.fsmonitor=false".to_owned(),
        "-c".to_owned(),
        "core.untrackedCache=false".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "pager.status=false".to_owned(),
        "status".to_owned(),
    ];
    argv.extend(status_arguments);
    Some(argv)
}

fn sanitize_git_environment(command: &mut Command, request: &CommandRequest) {
    for key in std::env::vars_os()
        .map(|(key, _)| key)
        .chain(request.env.keys().map(OsString::from))
    {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    for key in [
        "BASH_ENV",
        "ENV",
        "SHELLOPTS",
        "CDPATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "XDG_CONFIG_HOME",
    ] {
        command.env_remove(key);
    }
    command
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/dev/null")
        .env("XDG_CONFIG_HOME", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
}

fn configure_proxy_environment(command: &mut Command, proxy: Option<&SupervisedEgressProxy>) {
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    if let Some(proxy) = proxy {
        let url = proxy.url();
        command
            .env("HTTP_PROXY", &url)
            .env("HTTPS_PROXY", &url)
            .env("http_proxy", &url)
            .env("https_proxy", &url)
            .env("NO_PROXY", "")
            .env("no_proxy", "");
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
kill -KILL "-$1" 2>/dev/null || :
while kill -0 "-$1" 2>/dev/null; do sleep 0.01; done
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
                if macos_terminal_group_probe(error, raw_pid.as_raw_nonzero().get()).await {
                    return Ok(());
                }
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

#[cfg(target_os = "macos")]
async fn macos_terminal_group_probe(error: rustix::io::Errno, process_group: i32) -> bool {
    // Darwin may report EPERM for zombie-only groups, but EPERM can also mean
    // that a live member has different credentials. Never infer which case it
    // is from an earlier signal attempt; require an independent membership
    // snapshot that proves there are no executable members.
    error == rustix::io::Errno::PERM
        && matches!(
            macos_process_group_has_no_live_members(process_group).await,
            Some(true)
        )
}

#[cfg(target_os = "macos")]
async fn macos_process_group_has_no_live_members(process_group: i32) -> Option<bool> {
    const OUTPUT_CAP: usize = 256 * 1024;
    // Invoke the trusted absolute system binary without a shell or caller
    // environment, and bound every resource before treating its output as a
    // security decision. `None` is an unknown result and remains fail-closed.
    let mut command = Command::new("/bin/ps");
    command
        .args(["-axo", "pgid=,stat="])
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let collected = tokio::time::timeout(Duration::from_secs(2), async {
        let mut output = Vec::new();
        stdout
            .take((OUTPUT_CAP + 1) as u64)
            .read_to_end(&mut output)
            .await
            .ok()?;
        if output.len() > OUTPUT_CAP {
            return None;
        }
        let status = child.wait().await.ok()?;
        status.success().then_some(output)
    })
    .await;
    let Ok(Some(output)) = collected else {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_millis(100), child.wait()).await;
        return None;
    };
    parse_macos_process_group_status(&output, process_group)
}

#[cfg(target_os = "macos")]
fn parse_macos_process_group_status(output: &[u8], process_group: i32) -> Option<bool> {
    let output = std::str::from_utf8(output).ok()?;
    let mut saw_process = false;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_ascii_whitespace();
        let pgid = fields.next()?.parse::<i32>().ok()?;
        let status = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        saw_process = true;
        if pgid == process_group && !status.starts_with('Z') {
            return Some(false);
        }
    }
    saw_process.then_some(true)
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn macos_terminal_group_probe(_error: rustix::io::Errno, _process_group: i32) -> bool {
    false
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
    safety: Arc<CommandSafetyClassifier>,
    background: Option<Arc<BackgroundProcessManager>>,
}

impl BashTool {
    #[must_use]
    pub fn new(executor: Arc<dyn CommandExecutor>, limits: ToolLimits) -> Self {
        Self {
            executor,
            limits,
            safety: Arc::new(CommandSafetyClassifier::default()),
            background: None,
        }
    }

    #[must_use]
    pub fn with_background_manager(mut self, background: Arc<BackgroundProcessManager>) -> Self {
        self.background = Some(background);
        self
    }

    #[must_use]
    pub fn with_command_safety(mut self, safety: Arc<CommandSafetyClassifier>) -> Self {
        self.safety = safety;
        self
    }
}

#[async_trait]
impl Tool for BashTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "bash".to_owned(),
            description: "Run a sandboxed shell command with live stdout/stderr streaming, or supervise it in the background."
                .to_owned(),
            input_schema: input_schema::<BashInput>(),
            capabilities: CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::Network,
                ToolCapability::Execute,
            ]),
        }
    }

    fn invocation_capabilities(&self, input: &Value) -> Result<CapabilityManifest, ToolError> {
        let input: BashInput = parse_input(input.clone())?;
        Ok(if input.run_in_background {
            CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::Network,
                ToolCapability::Execute,
            ])
        } else {
            CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
                ToolCapability::Network,
                ToolCapability::Execute,
            ])
        })
    }

    fn mutation_scope(&self, input: &Value) -> crate::MutationScope {
        serde_json::from_value::<BashInput>(input.clone()).map_or(
            crate::MutationScope::OpaqueWorkspace,
            |input| {
                if input.run_in_background {
                    crate::MutationScope::None
                } else {
                    crate::MutationScope::OpaqueWorkspace
                }
            },
        )
    }

    async fn end_session(&self, session_id: &rw_types::SessionId) -> Result<(), ToolError> {
        if let Some(background) = &self.background {
            background.shutdown_session(session_id).await?;
        }
        Ok(())
    }

    fn session_activity(&self, session_id: &rw_types::SessionId) -> Option<String> {
        self.background
            .as_ref()
            .is_some_and(|background| background.has_running(session_id))
            .then(|| "background shell process is still running".to_owned())
    }

    fn observes_session_resources(&self) -> bool {
        true
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: BashInput = parse_input(input)?;
        if !input.run_in_background && input.sandbox == BashSandboxMode::ReadOnly {
            return Err(ToolError::InvalidInput(
                "read_only sandbox mode is reserved for supervised background commands".to_owned(),
            ));
        }
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
        let network_domains = normalize_requested_domains(&input.network_domains)?;
        let request = CommandRequest {
            network_domains,
            command: input.command,
            cwd,
            env: input.env,
            sandbox: input.sandbox,
        };
        if input.run_in_background {
            if input.sandbox != BashSandboxMode::Sandboxed {
                return Err(ToolError::InvalidInput(
                    "background commands must use the write-denied sandbox".to_owned(),
                ));
            }
            let mut request = request;
            request.sandbox = BashSandboxMode::ReadOnly;
            let manager = self.background.as_ref().ok_or_else(|| {
                ToolError::Command("background process manager is unavailable".to_owned())
            })?;
            let session_id = context.session_id().ok_or_else(|| {
                ToolError::Command("background commands require an actor-owned session".to_owned())
            })?;
            let process = manager.start(Arc::clone(&self.executor), session_id, request)?;
            if context.cancellation.is_cancelled() {
                let _ = manager.kill(session_id, &process.process_id).await;
                return Err(ToolError::Cancelled);
            }
            return Ok(ToolResult::new(
                format!("background process started: {}", process.process_id),
                json!({ "background_process": process }),
            ));
        }
        let outcome = self
            .executor
            .run(request, context.cancellation.clone(), capture.clone())
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

fn normalize_requested_domains(domains: &[String]) -> Result<Vec<String>, ToolError> {
    let mut normalized = domains
        .iter()
        .map(|domain| {
            normalize_egress_domain(domain).ok_or_else(|| {
                ToolError::InvalidInput(format!("invalid requested network domain {domain:?}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_group_status_accepts_only_absent_or_all_zombie_members() {
        assert_eq!(
            parse_macos_process_group_status(b"7 Ss\n42 Z\n42 Z+\n", 42),
            Some(true)
        );
        assert_eq!(
            parse_macos_process_group_status(b"7 Ss\n9 R\n", 42),
            Some(true)
        );
        assert_eq!(
            parse_macos_process_group_status(b"42 Z\n42 S\n", 42),
            Some(false)
        );
        assert_eq!(parse_macos_process_group_status(b"42\n", 42), None);
        assert_eq!(parse_macos_process_group_status(b"", 42), None);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn fake_eperm_with_a_live_group_remains_fail_closed() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn().expect("live process-group fixture");
        let process_group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .expect("fixture process group");
        assert!(
            !macos_terminal_group_probe(rustix::io::Errno::PERM, process_group).await,
            "EPERM with a demonstrably live member must remain fail-closed"
        );
        terminate_process_group(child.id());
        child.wait().await.expect("reap live process-group fixture");
    }

    #[cfg(unix)]
    #[test]
    fn nonblocking_execution_lease_refuses_an_existing_owner_immediately() {
        let root = tempdir().expect("temp directory");
        let path = root.path().join("execution.lock");
        let _owner = ExecutionLease::acquire(&path).expect("initial execution lease");
        let started = std::time::Instant::now();
        let error = ExecutionLease::try_acquire(&path).expect_err("second lease must fail");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(
            matches!(error, ToolError::Io { source, .. } if source.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn built_in_safe_list_accepts_only_plain_git_status() {
        for command in [
            "git status",
            "git status --short",
            "git status --porcelain=v1 -- .",
        ] {
            assert_eq!(
                classify_safe_command(command),
                CommandSafety::SafeListed,
                "expected safe-list classification for {command}"
            );
        }
        for command in [
            "git clean -fd",
            "git status && rm -rf /tmp/example",
            "git status; curl https://example.invalid",
            "git status $(touch escaped)",
            "git status `touch escaped`",
            "git -c alias.status='!touch escaped' status",
            "/usr/bin/git status",
            "./git status",
            "evil/git status",
            "PATH=. git status",
            "env PATH=. git status",
            "git status --help",
            "sh -c 'git status'",
            "",
        ] {
            assert_eq!(
                classify_safe_command(command),
                CommandSafety::RequiresApproval,
                "expected approval classification for {command}"
            );
        }
    }

    #[test]
    fn configured_safe_list_requires_every_conservative_compound_segment() {
        let classifier = CommandSafetyClassifier::new(&["cargo test*".to_owned()])
            .expect("configured classifier");
        for command in [
            "cargo test",
            "cargo test --workspace",
            "cargo test && cargo test --doc",
            "cargo test; cargo test --lib",
        ] {
            assert_eq!(classifier.classify(command), CommandSafety::SafeListed);
        }
        for command in [
            "cargo test && rm -rf target",
            "cargo test | tee output",
            "cargo test > output",
            "cargo test $(touch escaped)",
            "cargo test && 'unterminated",
        ] {
            assert_eq!(
                classifier.classify(command),
                CommandSafety::RequiresApproval,
                "unsafe compound was auto-safe: {command}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn general_commands_do_not_source_login_or_shell_control_profiles() {
        use std::os::unix::fs::PermissionsExt as _;

        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("root");
        let home = root.path().join("home");
        let trusted = root.path().join("trusted");
        let malicious = root.path().join("malicious");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&trusted).expect("trusted");
        std::fs::create_dir_all(&malicious).expect("malicious");
        let profile_canary = root.path().join("profile-ran");
        let result = root.path().join("result");
        std::fs::write(
            home.join(".profile"),
            format!(
                "printf profile > '{}'; export PATH='{}'\n",
                profile_canary.display(),
                malicious.display()
            ),
        )
        .expect("profile");
        for (directory, value) in [(&trusted, "trusted"), (&malicious, "malicious")] {
            let executable = directory.join("identity-probe");
            std::fs::write(
                &executable,
                format!("#!/bin/sh\nprintf {value} > \"$RESULT\"\n"),
            )
            .expect("probe");
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
                .expect("probe mode");
        }
        let outcome = TokioCommandExecutor::default()
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    command: "identity-probe".to_owned(),
                    cwd: root.path().to_path_buf(),
                    env: BTreeMap::from([
                        ("HOME".to_owned(), home.display().to_string()),
                        (
                            "PATH".to_owned(),
                            format!("{}:/usr/bin:/bin", trusted.display()),
                        ),
                        ("RESULT".to_owned(), result.display().to_string()),
                        (
                            "BASH_ENV".to_owned(),
                            home.join(".profile").display().to_string(),
                        ),
                        (
                            "ENV".to_owned(),
                            home.join(".profile").display().to_string(),
                        ),
                    ]),
                    network_domains: Vec::new(),
                },
                CancellationToken::default(),
                Arc::new(crate::NoopOutputSink),
            )
            .await
            .expect("command");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(std::fs::read_to_string(result).expect("result"), "trusted");
        assert!(!profile_canary.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_process_group_escape_launchers() {
        for command in [
            "setsid sh -c true",
            "/usr/bin/nohup true",
            "daemon --name canary",
            "unterminated '",
        ] {
            assert!(command_can_escape_process_group(command), "{command}");
        }
        assert!(!command_can_escape_process_group("printf ordinary"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandboxed_eperm_and_explicit_unsandboxed_escape_have_distinct_boundaries() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temporary directory");
        let workspace = root.path().join("workspace");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&scratch).expect("scratch");
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).expect("outside directory");
        std::fs::write(outside.join("canary"), "blocked").expect("outside canary");
        let policy = Arc::new(
            SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
                .expect("sandbox policy"),
        );
        let executor = TokioCommandExecutor::default()
            .sandboxed(policy)
            .with_policy_egress(true);
        let sink = Arc::new(RecordingSink::default());
        let command = format!(
            "printf allowed > allowed; rm -rf {}",
            shell_words::quote(&outside.to_string_lossy())
        );
        let outcome = executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command,
                    cwd: workspace.clone(),
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                sink.clone(),
            )
            .await
            .expect("guarded command outcome");
        assert_ne!(outcome.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(workspace.join("allowed")).expect("allowed write"),
            "allowed"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("canary")).expect("blocked outside canary"),
            "blocked"
        );
        let stderr = sink
            .0
            .lock()
            .expect("sink")
            .iter()
            .filter(|chunk| chunk.stream == ToolOutputStream::Stderr)
            .map(|chunk| chunk.content.as_str())
            .collect::<String>();
        assert!(
            stderr.contains("Operation not permitted"),
            "expected EPERM diagnostic, got {stderr:?}"
        );

        std::fs::remove_dir_all(&outside).expect("clean blocked canary");

        let escaped = executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Unsandboxed,
                    network_domains: Vec::new(),
                    command: format!("printf approved > '{}'", outside.display()),
                    cwd: workspace,
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("explicit unsandboxed command");
        assert_eq!(escaped.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside canary"),
            "approved"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandboxed_executor_denies_network_even_for_safe_list_eligible_processes() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temporary directory");
        let workspace = root.path().join("workspace");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&scratch).expect("scratch");
        let policy = Arc::new(
            SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
                .expect("sandbox policy"),
        );
        let probe = workspace.join("network-denial-probe.py");
        std::fs::write(
            &probe,
            r#"import errno, os, socket, sys
if any(os.environ.get(k) for k in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy")):
    sys.exit(94)
s = socket.socket()
try:
    s.connect(("127.0.0.1", 9))
except OSError as error:
    sys.exit(0 if error.errno in (errno.EPERM, errno.EACCES) else 93)
sys.exit(92)
"#,
        )
        .expect("network denial probe");
        let command = format!("python3 {}", shell_words::quote(&probe.to_string_lossy()));
        let classifier = Arc::new(
            CommandSafetyClassifier::new(&[globset::escape(&command)])
                .expect("test safe-list classifier"),
        );
        let executor = TokioCommandExecutor::default()
            .sandboxed(policy)
            .with_command_safety(Arc::clone(&classifier))
            .with_policy_egress(true);
        assert_eq!(classifier.classify(&command), CommandSafety::SafeListed);
        let outcome = executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command,
                    cwd: workspace,
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("guarded command outcome");
        assert_eq!(outcome.exit_code, 0, "network denial probe must see EPERM");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn requested_domains_receive_one_command_scoped_proxy_only() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temporary directory");
        let workspace = root.path().join("workspace");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&scratch).expect("scratch");
        let lifecycles = Arc::new(Mutex::new(Vec::new()));
        let executor = TokioCommandExecutor::default()
            .sandboxed(Arc::new(
                SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
                    .expect("sandbox policy"),
            ))
            .with_policy_egress(true)
            .with_proxy_lifecycle_observer(Arc::clone(&lifecycles));
        let sink = Arc::new(RecordingSink::default());
        let outcome = executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: vec!["example.com".to_owned()],
                    command: "printf '%s' \"$HTTPS_PROXY\"".to_owned(),
                    cwd: workspace,
                    env: BTreeMap::new(),
                },
                CancellationToken::default(),
                sink.clone(),
            )
            .await
            .expect("network-scoped command");
        assert_eq!(outcome.exit_code, 0);
        let output = sink
            .0
            .lock()
            .expect("sink")
            .iter()
            .filter(|chunk| chunk.stream == ToolOutputStream::Stdout)
            .map(|chunk| chunk.content.as_str())
            .collect::<String>();
        let _proxy = url::Url::parse(&output).expect("proxy URL");
        let observed = lifecycles.lock().expect("lifecycle observer");
        assert_eq!(observed.len(), 1);
        assert!(
            observed[0].is_stopped(),
            "per-command proxy listener supervisors were not joined"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn safe_listed_git_status_really_runs_inside_the_sandbox() {
        use std::os::unix::fs::PermissionsExt as _;

        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temporary directory");
        let workspace = root.path().join("workspace");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&scratch).expect("scratch");
        let git = audited_system_git().expect("audited system git");
        assert!(
            std::process::Command::new(git)
                .args(["init", "--quiet"])
                .current_dir(&workspace)
                .status()
                .expect("git init")
                .success()
        );
        let malicious_git = workspace.join("git");
        let executed = workspace.join("malicious-git-executed");
        std::fs::write(
            &malicious_git,
            format!(
                "#!/bin/sh\nprintf HOST_SECRET_CANARY\ntouch '{}'\n",
                executed.display()
            ),
        )
        .expect("malicious workspace git");
        std::fs::set_permissions(&malicious_git, std::fs::Permissions::from_mode(0o755))
            .expect("malicious git executable mode");
        assert!(
            std::process::Command::new(git)
                .args(["config", "core.fsmonitor", "./git"])
                .current_dir(&workspace)
                .status()
                .expect("malicious local git config")
                .success()
        );
        assert_eq!(
            classify_safe_command("git status --short"),
            CommandSafety::SafeListed
        );
        let executor = TokioCommandExecutor::default().sandboxed(Arc::new(
            SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
                .expect("sandbox policy"),
        ));
        let sink = Arc::new(RecordingSink::default());
        let outcome = executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command: "git status --short".to_owned(),
                    cwd: workspace.clone(),
                    env: BTreeMap::from([
                        ("PATH".to_owned(), workspace.display().to_string()),
                        ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
                        ("GIT_CONFIG_KEY_0".to_owned(), "core.fsmonitor".to_owned()),
                        ("GIT_CONFIG_VALUE_0".to_owned(), "./git".to_owned()),
                        ("BASH_ENV".to_owned(), malicious_git.display().to_string()),
                        ("ENV".to_owned(), malicious_git.display().to_string()),
                    ]),
                },
                CancellationToken::default(),
                sink.clone(),
            )
            .await
            .expect("sandboxed git status");
        assert_eq!(outcome.exit_code, 0);
        assert!(!executed.exists(), "workspace-controlled git was executed");
        let output = sink
            .0
            .lock()
            .expect("sink")
            .iter()
            .map(|chunk| chunk.content.as_str())
            .collect::<String>();
        assert!(!output.contains("HOST_SECRET_CANARY"), "{output:?}");
    }

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
        let recorded_env = BTreeMap::from([
            (
                "HOME".to_owned(),
                record_root.path().to_string_lossy().into_owned(),
            ),
            (
                "TMPDIR".to_owned(),
                record_root.path().to_string_lossy().into_owned(),
            ),
        ]);
        let expected_outcome = recorder
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command: dangerous_command.to_owned(),
                    cwd: record_root.path().to_path_buf(),
                    env: recorded_env,
                },
                CancellationToken::default(),
                recorded_sink.clone(),
            )
            .await
            .expect("record command");

        let offline_executor = ReplayCommandExecutor::load(fixtures.path(), replay_root.path())
            .expect("replay executor");
        let replayed_sink = Arc::new(RecordingSink::default());
        let replayed_env = BTreeMap::from([
            (
                "HOME".to_owned(),
                replay_root.path().to_string_lossy().into_owned(),
            ),
            (
                "TMPDIR".to_owned(),
                replay_root.path().to_string_lossy().into_owned(),
            ),
        ]);
        let actual_outcome = offline_executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command: dangerous_command.to_owned(),
                    cwd: replay_root.path().to_path_buf(),
                    env: replayed_env.clone(),
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
                        sandbox: BashSandboxMode::Sandboxed,
                        network_domains: Vec::new(),
                        command: dangerous_command.to_owned(),
                        cwd: replay_root.path().to_path_buf(),
                        env: replayed_env,
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
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
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
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
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
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
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
    async fn cancellation_before_launch_gate_never_releases_the_command() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temp directory");
        let sentinel = root.path().join("must-not-run");
        let command = format!(
            "printf launched > {}",
            shell_words::quote(sentinel.to_string_lossy().as_ref())
        );
        let cancellation = CancellationToken::default();
        let run_cancellation = cancellation.clone();
        let hook = Arc::new(LaunchGateTestHook::default());
        let run_hook = hook.clone();
        let executor = TokioCommandExecutor::default().with_launch_gate_hook(run_hook);
        let run = tokio::spawn(async move {
            executor
                .run(
                    CommandRequest {
                        sandbox: BashSandboxMode::Sandboxed,
                        network_domains: Vec::new(),
                        command,
                        cwd: root.path().to_path_buf(),
                        env: BTreeMap::new(),
                    },
                    run_cancellation,
                    Arc::new(crate::NoopOutputSink),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), hook.wait_until_reached())
            .await
            .expect("launch-gate barrier timeout");
        let child = hook.child_id().expect("guarded command pid");
        cancellation.cancel();
        hook.release();
        let outcome = tokio::time::timeout(Duration::from_secs(3), run)
            .await
            .expect("bounded pre-launch cancellation")
            .expect("executor join");
        assert!(
            matches!(outcome, Err(ToolError::Cancelled)),
            "unexpected pre-launch cancellation outcome: {outcome:?}"
        );
        assert!(!sentinel.exists(), "cancelled command was released");
        assert!(
            matches!(
                rustix::process::test_kill_process_group(child),
                Err(rustix::io::Errno::SRCH)
            ),
            "cancelled guarded command group survived"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_cancellation_kills_a_descendant_holding_the_pipes() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temp directory");
        let descendant_pid_file = root.path().join("descendant.pid");
        let command = format!(
            concat!(
                "sleep 30 & descendant=$!; ",
                "printf '%s\\n' \"$descendant\" > {}; ",
                "printf 'descendant-ready\\n'; wait"
            ),
            shell_words::quote(descendant_pid_file.to_string_lossy().as_ref())
        );
        let cancellation = CancellationToken::default();
        let run_cancellation = cancellation.clone();
        let executor = TokioCommandExecutor::default();
        let sink = Arc::new(RecordingSink::default());
        let run_sink = sink.clone();
        let run = tokio::spawn(async move {
            executor
                .run(
                    CommandRequest {
                        sandbox: BashSandboxMode::Sandboxed,
                        network_domains: Vec::new(),
                        command,
                        cwd: root.path().to_path_buf(),
                        env: BTreeMap::new(),
                    },
                    run_cancellation,
                    run_sink,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let ready = sink
                    .0
                    .lock()
                    .expect("recorded output")
                    .iter()
                    .map(|chunk| chunk.content.as_str())
                    .collect::<String>()
                    .contains("descendant-ready");
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("descendant readiness timeout");
        let descendant = std::fs::read_to_string(&descendant_pid_file)
            .expect("descendant pid file")
            .trim()
            .parse::<i32>()
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .expect("descendant pid");
        cancellation.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(3), run)
            .await
            .expect("bounded cancellation")
            .expect("executor join");
        assert!(
            matches!(outcome, Err(ToolError::Cancelled)),
            "unexpected cancellation outcome: {outcome:?}"
        );
        assert!(
            matches!(
                rustix::process::test_kill_process(descendant),
                Err(rustix::io::Errno::SRCH)
            ),
            "cancelled descendant survived process-group teardown"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_executor_disarms_and_reaps_watchdog_on_normal_completion() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temp directory");
        let sink = Arc::new(RecordingSink::default());
        let outcome = TokioCommandExecutor::default()
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
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
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temp directory");
        let pid_file = root.path().join("background.pid");
        let outcome = TokioCommandExecutor::default()
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
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
        use std::os::unix::fs::MetadataExt as _;

        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("temp directory");
        let lease = Arc::new(
            ExecutionLease::acquire(root.path().join("execution.lock")).expect("execution lease"),
        );
        let descriptor = lease.test_watchdog_raw_fd().to_string();
        let metadata = lease.file.metadata().expect("lease metadata");
        let device = metadata.dev().to_string();
        let inode = metadata.ino().to_string();
        let executable = std::env::current_exe().expect("current test executable");
        let unrelated = std::process::Command::new(&executable)
            .arg("--exact")
            .arg("bash::tests::lease_descriptor_probe_subprocess_helper")
            .arg("--nocapture")
            .env("ROTTWEILER_LEASE_PROBE_FD", &descriptor)
            .env("ROTTWEILER_LEASE_PROBE_DEV", &device)
            .env("ROTTWEILER_LEASE_PROBE_INO", &inode)
            .status()
            .expect("unrelated descriptor probe");
        assert!(unrelated.success(), "unrelated child inherited lease fd");

        let user_probe = format!(
            "{} --exact bash::tests::lease_descriptor_probe_subprocess_helper --nocapture",
            shell_words::quote(executable.to_string_lossy().as_ref())
        );
        let outcome = TokioCommandExecutor::with_execution_lease(lease)
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command: user_probe,
                    cwd: root.path().to_path_buf(),
                    env: BTreeMap::from([
                        ("ROTTWEILER_LEASE_PROBE_FD".to_owned(), descriptor),
                        ("ROTTWEILER_LEASE_PROBE_DEV".to_owned(), device),
                        ("ROTTWEILER_LEASE_PROBE_INO".to_owned(), inode),
                    ]),
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
    fn lease_descriptor_probe_subprocess_helper() {
        use std::os::unix::fs::MetadataExt as _;

        let Some(descriptor) = std::env::var_os("ROTTWEILER_LEASE_PROBE_FD") else {
            return;
        };
        let expected_device = std::env::var("ROTTWEILER_LEASE_PROBE_DEV")
            .expect("expected lease device")
            .parse::<u64>()
            .expect("numeric lease device");
        let expected_inode = std::env::var("ROTTWEILER_LEASE_PROBE_INO")
            .expect("expected lease inode")
            .parse::<u64>()
            .expect("numeric lease inode");
        let inherited = std::fs::metadata(format!("/dev/fd/{}", descriptor.to_string_lossy()))
            .is_ok_and(|metadata| {
                metadata.dev() == expected_device && metadata.ino() == expected_inode
            });
        if inherited {
            std::process::exit(90);
        }
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
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
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
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
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
            let watchdog_gone = !test_process_is_running(watchdog_pid);
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
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
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
            !test_process_is_running(watchdog_pid),
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let raw = loop {
            match tokio::fs::read_to_string(path).await {
                Ok(value) => {
                    if let Ok(pid) = value.trim().parse::<i32>() {
                        break pid;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("read pid file: {error}"),
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "numeric pid was not published before the deadline"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        rustix::process::Pid::from_raw(raw).expect("positive pid")
    }

    #[cfg(target_os = "linux")]
    fn test_process_is_running(pid: rustix::process::Pid) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero()))
        else {
            return false;
        };
        // The watchdog is orphaned when its executor is SIGKILLed. Linux can
        // retain the exited process as a zombie until PID 1 reaps it, during
        // which time kill(pid, 0) still reports success. The state is the first
        // field after the final ')' because comm may contain spaces.
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            return false;
        };
        !fields.starts_with('Z')
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn test_process_is_running(pid: rustix::process::Pid) -> bool {
        rustix::process::test_kill_process(pid).is_ok()
    }

    #[test]
    fn bash_declares_shared_capabilities_and_adds_write_only_for_foreground_calls() {
        let tool = BashTool::new(Arc::new(StreamingExecutor), ToolLimits::default());
        let descriptor = tool.descriptor();
        for capability in [
            ToolCapability::ReadFilesystem,
            ToolCapability::Network,
            ToolCapability::Execute,
        ] {
            assert!(descriptor.capabilities.contains(&capability));
        }
        assert!(
            !descriptor
                .capabilities
                .contains(&ToolCapability::WriteFilesystem)
        );
        assert!(
            tool.invocation_capabilities(&json!({ "command": "true" }))
                .expect("foreground capabilities")
                .contains(&ToolCapability::WriteFilesystem)
        );
        assert!(
            !tool
                .invocation_capabilities(&json!({ "command": "true", "run_in_background": true }))
                .expect("background capabilities")
                .contains(&ToolCapability::WriteFilesystem)
        );
    }
}
