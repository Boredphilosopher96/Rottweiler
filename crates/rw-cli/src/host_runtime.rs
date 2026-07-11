//! CLI composition for the headless multi-session engine host.

use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    AttachmentData, CommandDescriptor, CompletedForkOperation, Config, CreateSessionRequest,
    EngineEvent, ForkOperationKey, ForkOperationState, ForkSessionRequest, HostError,
    HostQueryService, HostedSession, ModelAlias, ModelCacheBehavior, ModelCapabilities,
    ModelDescriptor, PreparedForkOperation, SessionDescriptor, SessionFactory, SessionId,
    WorkspaceFileMatch, WorkspaceFilePreview, WorkspaceStatus, builtin_command_registry,
    project_session_events,
};
use rw_store::config::ConfigLoader;
use rw_store::session::{SessionEventLog, SessionIndex, UtcTimestamp};
use serde::{Deserialize, Serialize};

use crate::{
    PermissionMode,
    runtime::{
        HostedProviderMode, HostedSessionComposition, compose_hosted_actor,
        fork_hosted_session_storage, load_session_metadata_any, load_session_workspace_roots,
        new_session_id, remove_forked_session_storage,
    },
};

const MAX_SEARCH_RESULTS: usize = 1_000;
const MAX_SESSION_RESULTS: usize = 10_000;
#[cfg(unix)]
const MAX_SEARCH_ENTRIES: usize = 50_000;
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
const QUERY_DEADLINE: Duration = Duration::from_millis(100);
const FORK_JOURNAL_VERSION: u16 = 1;
const MAX_COMPLETED_FORK_OPERATIONS: usize = 4_096;
const MAX_PENDING_FORK_OPERATIONS: usize = 32;
const MAX_FORK_TEMP_FILES: usize = 16;
const MAX_FORK_JOURNAL_BYTES: usize = 256 * 1024;

fn unix_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ForkOperationJournal {
    version: u16,
    operation_id: String,
    stable_operation_id: String,
    client_id: rw_core::ClientId,
    request_id: rw_core::RequestId,
    payload_hash: String,
    updated_unix_ms: u64,
    parent: SessionDescriptor,
    child_model: ModelAlias,
    child_workspace_generation: u64,
    child_roots_digest: String,
    child_session_id: SessionId,
    at_turn: rw_core::TurnId,
    through_sequence: Option<rw_core::SequenceId>,
    include_idle_tail: bool,
    driver_client_id: rw_core::ClientId,
    canonical_workspace: PathBuf,
    workspace_digest: String,
    state: ForkJournalState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
enum ForkJournalState {
    Prepared,
    StorageCommitted,
    Completed { result: Box<ForkJournalResult> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ForkJournalResult {
    protocol_version: u16,
    command_ack_emitted_at: String,
    fork_event_emitted_at: String,
    acknowledged_session_id: SessionId,
    outcome: rw_core::CommandOutcome,
    parent_session_id: SessionId,
    child: SessionDescriptor,
    at_turn: rw_core::TurnId,
}

struct ExpectedForkState {
    model: ModelAlias,
    workspace_generation: u64,
    roots_digest: String,
}

impl ForkJournalState {
    fn rank(&self) -> u8 {
        match self {
            Self::Prepared => 0,
            Self::StorageCommitted => 1,
            Self::Completed { .. } => 2,
        }
    }
}

struct ForkJournalLock {
    #[cfg(unix)]
    _file: fs::File,
    #[cfg(not(unix))]
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[derive(Clone)]
pub(crate) struct CliHostOptions {
    pub storage_root: PathBuf,
    pub credentials_path: PathBuf,
    pub config: Config,
    pub allowed_workspaces: Vec<PathBuf>,
    pub permission_mode: Option<PermissionMode>,
    pub max_turns: usize,
    pub provider_mode: HostedProviderMode,
    pub dangerously_trust: bool,
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
            dangerously_trust,
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
        crate::runtime::initialize_private_storage_root(&options.storage_root)
            .map_err(|_| HostError::Persistence("host storage could not initialize".to_owned()))?;
        let factory = Self {
            options: Arc::new(options),
            allowed_workspaces: Arc::new(allowed),
        };
        factory.recover_fork_operations()?;
        Ok(factory)
    }

    fn fork_journal_directory(&self) -> PathBuf {
        self.options
            .storage_root
            .join("control")
            .join("fork-operations")
    }

    fn fork_operation_id(key: &ForkOperationKey) -> String {
        let mut input = b"rw-fork-operation-v1\0".to_vec();
        input.extend_from_slice(&(key.operation_id.len() as u64).to_be_bytes());
        input.extend_from_slice(key.operation_id.as_bytes());
        blake3::hash(&input).to_hex().to_string()
    }

    fn ensure_fork_journal_directory(&self) -> Result<PathBuf, HostError> {
        let control = self.options.storage_root.join("control");
        for directory in [&control, &self.fork_journal_directory()] {
            match fs::symlink_metadata(directory) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(HostError::Persistence(
                        "fork journal path is unsafe".to_owned(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match fs::create_dir(directory) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            let metadata = fs::symlink_metadata(directory).map_err(|_| {
                                HostError::Persistence(
                                    "fork journal path is unavailable".to_owned(),
                                )
                            })?;
                            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                                return Err(HostError::Persistence(
                                    "fork journal path is unsafe".to_owned(),
                                ));
                            }
                        }
                        Err(_) => {
                            return Err(HostError::Persistence(
                                "fork journal could not initialize".to_owned(),
                            ));
                        }
                    }
                }
                Err(_) => {
                    return Err(HostError::Persistence(
                        "fork journal path is unavailable".to_owned(),
                    ));
                }
            }
        }
        let directory = self.fork_journal_directory();
        #[cfg(unix)]
        fs::set_permissions(
            &directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .map_err(|_| HostError::Persistence("fork journal permissions failed".to_owned()))?;
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|_| HostError::Persistence("fork journal is unavailable".to_owned()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(HostError::Persistence(
                "fork journal path is unsafe".to_owned(),
            ));
        }
        Ok(directory)
    }

    fn acquire_fork_journal_lock(&self) -> Result<ForkJournalLock, HostError> {
        let directory = self.ensure_fork_journal_directory()?;
        let control = directory.parent().ok_or_else(|| {
            HostError::Persistence("fork control directory is unavailable".to_owned())
        })?;
        #[cfg(unix)]
        {
            let descriptor = rustix::fs::open(
                control.join("fork-operations.lock"),
                rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .map_err(|_| HostError::Persistence("fork journal lock is unsafe".to_owned()))?;
            let stat = rustix::fs::fstat(&descriptor).map_err(|_| {
                HostError::Persistence("fork journal lock is unavailable".to_owned())
            })?;
            if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
                || stat.st_nlink != 1
                || stat.st_mode & 0o077 != 0
            {
                return Err(HostError::Persistence(
                    "fork journal lock is not private and regular".to_owned(),
                ));
            }
            let file = fs::File::from(descriptor);
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
                .map_err(|_| HostError::Persistence("fork journal lock failed".to_owned()))?;
            Ok(ForkJournalLock { _file: file })
        }
        #[cfg(not(unix))]
        {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            Ok(ForkJournalLock {
                _guard: LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            })
        }
    }

    fn is_lower_hex(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn expected_fork_state(
        &self,
        request: &ForkSessionRequest,
        workspace: &Path,
    ) -> Result<ExpectedForkState, HostError> {
        let metadata =
            load_session_metadata_any(&self.options.storage_root, &request.parent.session_id.0)
                .map_err(|_| {
                    HostError::Persistence("fork parent metadata is unavailable".to_owned())
                })?;
        if metadata.workspace != workspace {
            return Err(HostError::Persistence(
                "fork parent workspace does not match its operation".to_owned(),
            ));
        }
        let envelopes = SessionEventLog::load_existing_bounded::<EngineEvent>(
            &self.options.storage_root,
            &request.parent.session_id.0,
            crate::history::MAX_HISTORY_BYTES,
            crate::history::MAX_HISTORY_EVENTS,
        )
        .map_err(|_| HostError::Persistence("fork parent event log is unavailable".to_owned()))?;
        let fork_turn = request
            .at_turn
            .0
            .parse::<u64>()
            .map_err(|_| HostError::Persistence("fork turn is invalid".to_owned()))?;
        let boundary = if request.include_idle_tail {
            request.through_sequence
        } else if fork_turn == 0 {
            None
        } else {
            Some(
                envelopes
                    .iter()
                    .rev()
                    .find_map(|envelope| match &envelope.event {
                        EngineEvent::TurnFinished { turn_id, .. }
                            if *turn_id == request.at_turn =>
                        {
                            Some(envelope.sequence)
                        }
                        _ => None,
                    })
                    .ok_or_else(|| {
                        HostError::Persistence("fork turn boundary is unavailable".to_owned())
                    })?,
            )
        };
        let inherited_count = boundary.map_or(Ok(0_usize), |sequence| {
            usize::try_from(sequence.0)
                .map_err(|_| HostError::Persistence("fork boundary is invalid".to_owned()))?
                .checked_add(1)
                .ok_or_else(|| HostError::Persistence("fork boundary is invalid".to_owned()))
        })?;
        let inherited = envelopes.get(..inherited_count).ok_or_else(|| {
            HostError::Persistence("fork boundary exceeds its event log".to_owned())
        })?;
        let recovered = project_session_events(
            &inherited
                .iter()
                .map(|envelope| envelope.event.clone())
                .collect::<Vec<_>>(),
        )
        .map_err(|_| HostError::Persistence("fork event projection failed".to_owned()))?;
        let roots = crate::runtime::load_checkpoint_roots_exact(
            &crate::runtime::checkpoint_root(
                &self.options.storage_root,
                workspace,
                &request.parent.session_id.0,
            ),
            recovered.workspace_generation,
        )
        .map_err(|_| HostError::Persistence("fork root generation is unavailable".to_owned()))?
        .ok_or_else(|| {
            HostError::Persistence("fork root generation is not committed".to_owned())
        })?;
        let roots_digest =
            blake3::hash(&serde_json::to_vec(&roots).map_err(|_| {
                HostError::Persistence("fork roots could not serialize".to_owned())
            })?)
            .to_hex()
            .to_string();
        Ok(ExpectedForkState {
            model: ModelAlias(recovered.model_alias.unwrap_or(metadata.model_alias)),
            workspace_generation: recovered.workspace_generation,
            roots_digest,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn validate_fork_journal(
        &self,
        journal: &ForkOperationJournal,
        path: &Path,
    ) -> Result<(), HostError> {
        let key = ForkOperationKey {
            operation_id: journal.stable_operation_id.clone(),
            client_id: journal.client_id.clone(),
            request_id: journal.request_id.clone(),
            payload_hash: journal.payload_hash.clone(),
        };
        let expected_id = Self::fork_operation_id(&key);
        let expected_filename = format!("{expected_id}.json");
        let canonical = self.authorize_workspace_path(&journal.canonical_workspace)?;
        let workspace_digest = blake3::hash(canonical.as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string();
        let safe_text = |value: &str| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        };
        let safe_session = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        };
        let safe_operation = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        };
        if journal.version != FORK_JOURNAL_VERSION
            || !Self::is_lower_hex(&journal.operation_id)
            || journal.operation_id != expected_id
            || path.file_name().and_then(|name| name.to_str()) != Some(&expected_filename)
            || !Self::is_lower_hex(&journal.payload_hash)
            || !Self::is_lower_hex(&journal.workspace_digest)
            || !Self::is_lower_hex(&journal.child_roots_digest)
            || journal.workspace_digest != workspace_digest
            || canonical != journal.canonical_workspace
            || !safe_text(&journal.client_id.0)
            || !safe_text(&journal.request_id.0)
            || !safe_operation(&journal.stable_operation_id)
            || !safe_text(&journal.child_model.0)
            || !safe_session(&journal.parent.session_id.0)
            || !safe_session(&journal.child_session_id.0)
            || journal.driver_client_id != journal.client_id
            || journal.at_turn.0.parse::<u64>().is_err()
            || workspace_name(&canonical) != journal.parent.workspace_name
        {
            return Err(HostError::Persistence(
                "fork journal validation failed".to_owned(),
            ));
        }
        if let ForkJournalState::Completed { result } = &journal.state {
            crate::runtime::validate_forked_session_commit(
                &self.options.storage_root,
                &journal.canonical_workspace,
                &journal.child_session_id.0,
                &journal.operation_id,
                &journal.parent.session_id.0,
            )
            .map_err(|_| {
                HostError::Persistence("completed fork storage validation failed".to_owned())
            })?;
            let child_metadata =
                load_session_metadata_any(&self.options.storage_root, &journal.child_session_id.0)
                    .map_err(|_| {
                        HostError::Persistence("completed fork metadata is unavailable".to_owned())
                    })?;
            let child_roots_digest = blake3::hash(
                &serde_json::to_vec(&child_metadata.workspace_roots).map_err(|_| {
                    HostError::Persistence("completed fork roots could not serialize".to_owned())
                })?,
            )
            .to_hex()
            .to_string();
            if result.protocol_version != rw_core::PROTOCOL_VERSION
                || UtcTimestamp::parse(result.command_ack_emitted_at.clone()).is_err()
                || UtcTimestamp::parse(result.fork_event_emitted_at.clone()).is_err()
                || result.outcome != rw_core::CommandOutcome::Accepted
                || result.acknowledged_session_id != journal.parent.session_id
                || result.parent_session_id != journal.parent.session_id
                || result.child.session_id != journal.child_session_id
                || result.child.workspace_name != journal.parent.workspace_name
                || result.child.model != journal.child_model
                || child_metadata.workspace_generation != journal.child_workspace_generation
                || child_roots_digest != journal.child_roots_digest
                || result
                    .child
                    .driver_client_id
                    .as_ref()
                    .is_none_or(|driver| !safe_text(&driver.0))
                || result.child.shell_active
                || !safe_text(&result.child.workspace_name)
                || !safe_text(&result.child.model.0)
                || result.at_turn != journal.at_turn
            {
                return Err(HostError::Persistence(
                    "completed fork result does not match its prepared operation".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn fork_journal_path(&self, key: &ForkOperationKey) -> Result<PathBuf, HostError> {
        Ok(self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", Self::fork_operation_id(key))))
    }

    #[cfg(unix)]
    fn read_fork_journal_file(&self, filename: &std::ffi::OsStr) -> Result<Vec<u8>, HostError> {
        let root = rustix::fs::open(
            &self.options.storage_root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("storage root is unsafe".to_owned()))?;
        let control = rustix::fs::openat(
            &root,
            "control",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork control directory is unsafe".to_owned()))?;
        let directory = rustix::fs::openat(
            &control,
            "fork-operations",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork journal directory is unsafe".to_owned()))?;
        let file = rustix::fs::openat(
            &directory,
            filename,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork journal file is unsafe".to_owned()))?;
        let stat = rustix::fs::fstat(&file)
            .map_err(|_| HostError::Persistence("fork journal metadata failed".to_owned()))?;
        if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
            || stat.st_nlink != 1
            || stat.st_mode & 0o077 != 0
            || usize::try_from(stat.st_size).unwrap_or(usize::MAX) > MAX_FORK_JOURNAL_BYTES
        {
            return Err(HostError::Persistence(
                "fork journal file is not private and regular".to_owned(),
            ));
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(stat.st_size)
                .unwrap_or(MAX_FORK_JOURNAL_BYTES)
                .min(MAX_FORK_JOURNAL_BYTES),
        );
        fs::File::from(file)
            .take((MAX_FORK_JOURNAL_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| HostError::Persistence("fork journal could not read".to_owned()))?;
        if bytes.len() > MAX_FORK_JOURNAL_BYTES {
            return Err(HostError::Persistence(
                "fork journal exceeds its byte limit".to_owned(),
            ));
        }
        Ok(bytes)
    }

    #[cfg(not(unix))]
    fn read_fork_journal_file(&self, filename: &std::ffi::OsStr) -> Result<Vec<u8>, HostError> {
        let path = self.ensure_fork_journal_directory()?.join(filename);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| HostError::Persistence("fork journal file is unavailable".to_owned()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || usize::try_from(metadata.len()).unwrap_or(usize::MAX) > MAX_FORK_JOURNAL_BYTES
        {
            return Err(HostError::Persistence(
                "fork journal file is unsafe".to_owned(),
            ));
        }
        fs::read(path).map_err(|_| HostError::Persistence("fork journal could not read".to_owned()))
    }

    fn load_fork_journal_unlocked(
        &self,
        key: &ForkOperationKey,
    ) -> Result<Option<ForkOperationJournal>, HostError> {
        let path = self.fork_journal_path(key)?;
        if !path.exists() {
            return Ok(None);
        }
        let filename = path.file_name().ok_or_else(|| {
            HostError::Persistence("fork journal filename is unavailable".to_owned())
        })?;
        let bytes = self.read_fork_journal_file(filename)?;
        let journal: ForkOperationJournal = serde_json::from_slice(&bytes)
            .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
        self.validate_fork_journal(&journal, &path)?;
        let operation_id = Self::fork_operation_id(key);
        if journal.version != FORK_JOURNAL_VERSION
            || journal.operation_id != operation_id
            || journal.stable_operation_id != key.operation_id
        {
            return Err(HostError::Persistence(
                "fork journal identity is corrupt".to_owned(),
            ));
        }
        if journal.payload_hash != key.payload_hash {
            return Err(HostError::RequestConflict);
        }
        Ok(Some(journal))
    }

    fn load_fork_journal(
        &self,
        key: &ForkOperationKey,
    ) -> Result<Option<ForkOperationJournal>, HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        self.load_fork_journal_unlocked(key)
    }

    fn persist_fork_journal(
        path: &Path,
        journal: &ForkOperationJournal,
        replace: bool,
    ) -> Result<(), HostError> {
        let bytes = serde_json::to_vec_pretty(journal)
            .map_err(|_| HostError::Persistence("fork journal could not serialize".to_owned()))?;
        if bytes.len() > MAX_FORK_JOURNAL_BYTES {
            return Err(HostError::Persistence(
                "fork journal exceeds its byte limit".to_owned(),
            ));
        }
        let directory = path.parent().ok_or_else(|| {
            HostError::Persistence("fork journal directory is unavailable".to_owned())
        })?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = directory.join(format!(".fork-{}-{nonce}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| HostError::Persistence("fork journal could not create".to_owned()))?;
        let result = (|| {
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| HostError::Persistence("fork journal could not sync".to_owned()))?;
            #[cfg(unix)]
            {
                let directory_file = fs::File::open(directory).map_err(|_| {
                    HostError::Persistence("fork journal directory is unavailable".to_owned())
                })?;
                let temporary_name = temporary.file_name().ok_or_else(|| {
                    HostError::Persistence("fork journal temporary name is unavailable".to_owned())
                })?;
                let final_name = path.file_name().ok_or_else(|| {
                    HostError::Persistence("fork journal filename is unavailable".to_owned())
                })?;
                let rename = if replace {
                    rustix::fs::renameat(
                        &directory_file,
                        temporary_name,
                        &directory_file,
                        final_name,
                    )
                } else {
                    rustix::fs::renameat_with(
                        &directory_file,
                        temporary_name,
                        &directory_file,
                        final_name,
                        rustix::fs::RenameFlags::NOREPLACE,
                    )
                };
                rename.map_err(|error| {
                    if !replace && error == rustix::io::Errno::EXIST {
                        HostError::RequestConflict
                    } else {
                        HostError::Persistence("fork journal update could not commit".to_owned())
                    }
                })?;
                rustix::fs::fsync(&directory_file).map_err(|_| {
                    HostError::Persistence("fork journal directory could not sync".to_owned())
                })?;
            }
            #[cfg(not(unix))]
            {
                if !replace && path.exists() {
                    return Err(HostError::RequestConflict);
                }
                fs::rename(&temporary, path).map_err(|_| {
                    HostError::Persistence("fork journal update could not commit".to_owned())
                })?;
                fs::File::open(directory)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|_| {
                        HostError::Persistence("fork journal directory could not sync".to_owned())
                    })?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn persist_new_fork_journal(
        path: &Path,
        journal: &ForkOperationJournal,
    ) -> Result<(), HostError> {
        Self::persist_fork_journal(path, journal, false)
    }

    fn same_fork_operation(left: &ForkOperationJournal, right: &ForkOperationJournal) -> bool {
        left.version == right.version
            && left.operation_id == right.operation_id
            && left.child_model == right.child_model
            && left.child_workspace_generation == right.child_workspace_generation
            && left.child_roots_digest == right.child_roots_digest
            && Self::journal_operation(left) == Self::journal_operation(right)
            && left.canonical_workspace == right.canonical_workspace
            && left.workspace_digest == right.workspace_digest
    }

    fn transition_fork_journal_unlocked(
        &self,
        candidate: &ForkOperationJournal,
    ) -> Result<ForkOperationJournal, HostError> {
        let key = ForkOperationKey {
            operation_id: candidate.stable_operation_id.clone(),
            client_id: candidate.client_id.clone(),
            request_id: candidate.request_id.clone(),
            payload_hash: candidate.payload_hash.clone(),
        };
        let current = self
            .load_fork_journal_unlocked(&key)?
            .ok_or_else(|| HostError::Persistence("fork operation was not prepared".to_owned()))?;
        if !Self::same_fork_operation(&current, candidate) {
            return Err(HostError::RequestConflict);
        }
        let current_rank = current.state.rank();
        let candidate_rank = candidate.state.rank();
        if current_rank >= candidate_rank {
            return Ok(current);
        }
        if candidate_rank != current_rank.saturating_add(1) {
            return Err(HostError::Persistence(
                "fork journal transition skipped a durable phase".to_owned(),
            ));
        }
        let path = self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", candidate.operation_id));
        self.validate_fork_journal(candidate, &path)?;
        Self::persist_fork_journal(&path, candidate, true)?;
        Ok(candidate.clone())
    }

    #[cfg(test)]
    fn force_replace_fork_journal_for_test(
        &self,
        journal: &ForkOperationJournal,
    ) -> Result<(), HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        let path = self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", journal.operation_id));
        Self::persist_fork_journal(&path, journal, true)
    }

    #[cfg(test)]
    fn transition_fork_journal_for_test(
        &self,
        journal: &ForkOperationJournal,
    ) -> Result<ForkOperationJournal, HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        self.transition_fork_journal_unlocked(journal)
    }

    fn journal_operation(journal: &ForkOperationJournal) -> PreparedForkOperation {
        PreparedForkOperation {
            key: ForkOperationKey {
                operation_id: journal.stable_operation_id.clone(),
                client_id: journal.client_id.clone(),
                request_id: journal.request_id.clone(),
                payload_hash: journal.payload_hash.clone(),
            },
            request: ForkSessionRequest {
                operation_key: ForkOperationKey {
                    operation_id: journal.stable_operation_id.clone(),
                    client_id: journal.client_id.clone(),
                    request_id: journal.request_id.clone(),
                    payload_hash: journal.payload_hash.clone(),
                },
                parent: journal.parent.clone(),
                child_session_id: journal.child_session_id.clone(),
                at_turn: journal.at_turn.clone(),
                through_sequence: journal.through_sequence,
                include_idle_tail: journal.include_idle_tail,
                driver_client_id: journal.driver_client_id.clone(),
            },
        }
    }

    fn completed_fork_result(result: &ForkJournalResult) -> CompletedForkOperation {
        CompletedForkOperation {
            protocol_version: result.protocol_version,
            command_ack_emitted_at: result.command_ack_emitted_at.clone(),
            fork_event_emitted_at: result.fork_event_emitted_at.clone(),
            acknowledged_session_id: result.acknowledged_session_id.clone(),
            outcome: result.outcome.clone(),
            parent_session_id: result.parent_session_id.clone(),
            child: result.child.clone(),
            at_turn: result.at_turn.clone(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn recover_fork_operations(&self) -> Result<(), HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        let directory = self.ensure_fork_journal_directory()?;
        let mut completed = Vec::new();
        let mut pending = 0_usize;
        let mut seen = 0_usize;
        let mut directory_changed = false;
        let entries = fs::read_dir(&directory)
            .map_err(|_| HostError::Persistence("fork journal could not scan".to_owned()))?;
        for entry in entries {
            let entry =
                entry.map_err(|_| HostError::Persistence("fork journal scan failed".to_owned()))?;
            let path = entry.path();
            seen = seen.saturating_add(1);
            if seen
                > MAX_COMPLETED_FORK_OPERATIONS + MAX_PENDING_FORK_OPERATIONS + MAX_FORK_TEMP_FILES
            {
                return Err(HostError::Persistence(
                    "fork journal exceeds its bounded capacity".to_owned(),
                ));
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                HostError::Persistence("fork journal has a non-Unicode entry".to_owned())
            })?;
            if name.starts_with(".fork-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
            {
                let metadata = fs::symlink_metadata(&path).map_err(|_| {
                    HostError::Persistence("fork journal temporary file is unsafe".to_owned())
                })?;
                #[cfg(unix)]
                let private_single_link = {
                    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                    metadata.nlink() == 1 && metadata.permissions().mode().trailing_zeros() >= 6
                };
                #[cfg(not(unix))]
                let private_single_link = true;
                if metadata.file_type().is_symlink() || !metadata.is_file() || !private_single_link
                {
                    return Err(HostError::Persistence(
                        "fork journal temporary file is unsafe".to_owned(),
                    ));
                }
                fs::remove_file(path).map_err(|_| {
                    HostError::Persistence("fork journal temporary cleanup failed".to_owned())
                })?;
                directory_changed = true;
                continue;
            }
            let stem = name.strip_suffix(".json").ok_or_else(|| {
                HostError::Persistence("fork journal has an unexpected entry".to_owned())
            })?;
            if !Self::is_lower_hex(stem) {
                return Err(HostError::Persistence(
                    "fork journal filename is invalid".to_owned(),
                ));
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| {
                HostError::Persistence("fork journal entry is unavailable".to_owned())
            })?;
            #[cfg(unix)]
            let single_link = {
                use std::os::unix::fs::MetadataExt as _;
                metadata.nlink() == 1
            };
            #[cfg(not(unix))]
            let single_link = true;
            if metadata.file_type().is_symlink() || !metadata.is_file() || !single_link {
                return Err(HostError::Persistence(
                    "fork journal entry is unsafe".to_owned(),
                ));
            }
            let bytes = self.read_fork_journal_file(&entry.file_name())?;
            let mut journal: ForkOperationJournal = serde_json::from_slice(&bytes)
                .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
            self.validate_fork_journal(&journal, &path)?;
            match journal.state {
                ForkJournalState::Prepared => {
                    let metadata = self
                        .options
                        .storage_root
                        .join("sessions")
                        .join(&journal.child_session_id.0)
                        .join("metadata.json");
                    if metadata.is_file() {
                        crate::runtime::validate_forked_session_commit(
                            &self.options.storage_root,
                            &journal.canonical_workspace,
                            &journal.child_session_id.0,
                            &journal.operation_id,
                            &journal.parent.session_id.0,
                        )
                        .map_err(|_| {
                            HostError::Persistence(
                                "committed fork storage failed recovery validation".to_owned(),
                            )
                        })?;
                        journal.state = ForkJournalState::StorageCommitted;
                        journal.updated_unix_ms = unix_millis();
                        self.transition_fork_journal_unlocked(&journal)?;
                        pending = pending.saturating_add(1);
                    } else {
                        remove_forked_session_storage(
                            &self.options.storage_root,
                            &journal.canonical_workspace,
                            &journal.child_session_id.0,
                        )
                        .map_err(|_| {
                            HostError::Persistence("partial fork cleanup failed".to_owned())
                        })?;
                        // The durable operation remains authoritative even before
                        // child metadata exists, so retry reuses the same child id.
                        pending = pending.saturating_add(1);
                    }
                }
                ForkJournalState::StorageCommitted => {
                    crate::runtime::validate_forked_session_commit(
                        &self.options.storage_root,
                        &journal.canonical_workspace,
                        &journal.child_session_id.0,
                        &journal.operation_id,
                        &journal.parent.session_id.0,
                    )
                    .map_err(|_| {
                        HostError::Persistence(
                            "committed fork storage failed recovery validation".to_owned(),
                        )
                    })?;
                    pending = pending.saturating_add(1);
                }
                ForkJournalState::Completed { .. } => {
                    crate::runtime::validate_forked_session_commit(
                        &self.options.storage_root,
                        &journal.canonical_workspace,
                        &journal.child_session_id.0,
                        &journal.operation_id,
                        &journal.parent.session_id.0,
                    )
                    .map_err(|_| {
                        HostError::Persistence(
                            "completed fork storage failed recovery validation".to_owned(),
                        )
                    })?;
                    completed.push((journal.updated_unix_ms, journal.operation_id, path));
                }
            }
        }
        if pending > MAX_PENDING_FORK_OPERATIONS {
            return Err(HostError::Persistence(
                "too many unfinished fork operations require recovery".to_owned(),
            ));
        }
        completed.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        let remove = completed
            .len()
            .saturating_sub(MAX_COMPLETED_FORK_OPERATIONS);
        for (_, _, path) in completed.into_iter().take(remove) {
            fs::remove_file(path)
                .map_err(|_| HostError::Persistence("fork journal retention failed".to_owned()))?;
            directory_changed = true;
        }
        if directory_changed {
            fs::File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| {
                    HostError::Persistence("fork journal cleanup could not sync".to_owned())
                })?;
        }
        Ok(())
    }

    fn enforce_live_fork_limits_unlocked(&self, prune_completed: bool) -> Result<(), HostError> {
        let directory = self.ensure_fork_journal_directory()?;
        let mut pending = 0_usize;
        let mut completed = Vec::new();
        let mut seen = 0_usize;
        for entry in fs::read_dir(&directory)
            .map_err(|_| HostError::Persistence("fork journal could not scan".to_owned()))?
        {
            let entry =
                entry.map_err(|_| HostError::Persistence("fork journal scan failed".to_owned()))?;
            seen = seen.saturating_add(1);
            if seen
                > MAX_COMPLETED_FORK_OPERATIONS + MAX_PENDING_FORK_OPERATIONS + MAX_FORK_TEMP_FILES
            {
                return Err(HostError::Persistence(
                    "fork journal exceeds its bounded capacity".to_owned(),
                ));
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let journal: ForkOperationJournal =
                serde_json::from_slice(&self.read_fork_journal_file(&entry.file_name())?)
                    .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
            self.validate_fork_journal(&journal, &path)?;
            match journal.state {
                ForkJournalState::Completed { .. } => {
                    completed.push((journal.updated_unix_ms, journal.operation_id, path));
                }
                ForkJournalState::Prepared | ForkJournalState::StorageCommitted => {
                    pending = pending.saturating_add(1);
                }
            }
        }
        if !prune_completed && pending >= MAX_PENDING_FORK_OPERATIONS {
            return Err(HostError::SessionCapacity);
        }
        if prune_completed && completed.len() > MAX_COMPLETED_FORK_OPERATIONS {
            completed.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
            let remove = completed.len() - MAX_COMPLETED_FORK_OPERATIONS;
            for (_, _, path) in completed.into_iter().take(remove) {
                fs::remove_file(path).map_err(|_| {
                    HostError::Persistence("fork journal retention failed".to_owned())
                })?;
            }
            fs::File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| {
                    HostError::Persistence("fork journal retention could not sync".to_owned())
                })?;
        }
        Ok(())
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
            let canonical = self.authorize_workspace_path(&root)?;
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
            additional_workspaces: Vec::new(),
            allowed_workspace_roots: self
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
            dangerously_trust: self.options.dangerously_trust,
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

    fn search_sessions_blocking(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<SessionDescriptor>, bool), HostError> {
        let requested = usize::try_from(limit)
            .map_err(|_| HostError::Query("session search limit is unsupported".to_owned()))?;
        let rows = SessionIndex::search_read_only(
            &self.options.storage_root,
            query,
            requested.saturating_add(1),
        )
        .map_err(|_| HostError::Query("session index search failed".to_owned()))?;
        let truncated = rows.len() > requested;
        let descriptors = rows
            .into_iter()
            .take(requested)
            .filter_map(|row| self.persisted_descriptor(&row.id).ok())
            .collect();
        Ok((descriptors, truncated))
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

    async fn load_fork_operation(
        &self,
        key: &ForkOperationKey,
    ) -> Result<ForkOperationState, HostError> {
        let factory = self.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let Some(journal) = factory.load_fork_journal_unlocked(&key)? else {
                return Ok(ForkOperationState::Missing);
            };
            match journal.state {
                ForkJournalState::Prepared | ForkJournalState::StorageCommitted => Ok(
                    ForkOperationState::Pending(Self::journal_operation(&journal)),
                ),
                ForkJournalState::Completed { result } => {
                    Ok(ForkOperationState::Completed(CompletedForkOperation {
                        protocol_version: result.protocol_version,
                        command_ack_emitted_at: result.command_ack_emitted_at,
                        fork_event_emitted_at: result.fork_event_emitted_at,
                        acknowledged_session_id: result.acknowledged_session_id,
                        outcome: result.outcome,
                        parent_session_id: result.parent_session_id,
                        child: result.child,
                        at_turn: result.at_turn,
                    }))
                }
            }
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn prepare_fork_operation(
        &self,
        operation: PreparedForkOperation,
    ) -> Result<PreparedForkOperation, HostError> {
        let workspace = self.workspace_for_session(&operation.request.parent)?;
        let factory = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            if let Some(existing) = factory.load_fork_journal_unlocked(&operation.key)? {
                return Ok(Self::journal_operation(&existing));
            }
            factory.enforce_live_fork_limits_unlocked(false)?;
            if operation.request.operation_key != operation.key {
                return Err(HostError::Protocol(
                    "fork operation key does not match its request".to_owned(),
                ));
            }
            let operation_id = Self::fork_operation_id(&operation.key);
            let child = factory.expected_fork_state(&operation.request, &workspace)?;
            let journal = ForkOperationJournal {
                version: FORK_JOURNAL_VERSION,
                operation_id,
                stable_operation_id: operation.key.operation_id.clone(),
                client_id: operation.key.client_id.clone(),
                request_id: operation.key.request_id.clone(),
                payload_hash: operation.key.payload_hash.clone(),
                updated_unix_ms: unix_millis(),
                parent: operation.request.parent.clone(),
                child_model: child.model,
                child_workspace_generation: child.workspace_generation,
                child_roots_digest: child.roots_digest,
                child_session_id: operation.request.child_session_id.clone(),
                at_turn: operation.request.at_turn.clone(),
                through_sequence: operation.request.through_sequence,
                include_idle_tail: operation.request.include_idle_tail,
                driver_client_id: operation.request.driver_client_id.clone(),
                workspace_digest: blake3::hash(workspace.as_os_str().as_encoded_bytes())
                    .to_hex()
                    .to_string(),
                canonical_workspace: workspace,
                state: ForkJournalState::Prepared,
            };
            let path = factory.fork_journal_path(&operation.key)?;
            match Self::persist_new_fork_journal(&path, &journal) {
                Ok(()) => Ok(operation),
                Err(HostError::RequestConflict) => factory
                    .load_fork_journal_unlocked(&operation.key)?
                    .map(|existing| Self::journal_operation(&existing))
                    .ok_or_else(|| {
                        HostError::Persistence("fork journal creation raced".to_owned())
                    }),
                Err(error) => Err(error),
            }
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn fork(&self, request: ForkSessionRequest) -> Result<HostedSession, HostError> {
        let workspace = self.workspace_for_session(&request.parent)?;
        let through_turn =
            request.at_turn.0.parse::<u64>().map_err(|_| {
                HostError::Protocol("fork turn must be an unsigned decimal".to_owned())
            })?;
        let storage_root = self.options.storage_root.clone();
        let workspace_for_fork = workspace.clone();
        let parent_session_id = request.parent.session_id.0.clone();
        let child_session = request.child_session_id.clone();
        let child_session_id = child_session.0.clone();
        let fork_child_session_id = child_session_id.clone();
        let through_sequence = request.through_sequence;
        let include_idle_tail = request.include_idle_tail;
        let driver_client_id = request.driver_client_id.clone();
        let operation_key = request.operation_key.clone();
        let factory = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let mut journal = factory
                .load_fork_journal_unlocked(&operation_key)?
                .ok_or_else(|| {
                    HostError::Persistence("fork operation was not prepared".to_owned())
                })?;
            if Self::journal_operation(&journal).request != request {
                return Err(HostError::RequestConflict);
            }
            if matches!(journal.state, ForkJournalState::Prepared) {
                let operation_id = journal.operation_id.clone();
                fork_hosted_session_storage(
                    &storage_root,
                    &workspace_for_fork,
                    &parent_session_id,
                    &fork_child_session_id,
                    through_turn,
                    through_sequence,
                    include_idle_tail,
                    driver_client_id,
                    Some(&operation_id),
                )
                .map_err(|error| {
                    tracing::error!(reason = %error, "session fork storage failed");
                    HostError::Persistence("session fork could not be persisted".to_owned())
                })?;
                journal.state = ForkJournalState::StorageCommitted;
                journal.updated_unix_ms = unix_millis();
                factory.transition_fork_journal_unlocked(&journal)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| HostError::Persistence("fork storage worker failed".to_owned()))??;
        self.compose(child_session, workspace, None, true).await
    }

    async fn complete_fork_operation(
        &self,
        key: &ForkOperationKey,
        result: &CompletedForkOperation,
    ) -> Result<CompletedForkOperation, HostError> {
        let factory = self.clone();
        let key = key.clone();
        let result = result.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let mut journal = factory.load_fork_journal_unlocked(&key)?.ok_or_else(|| {
                HostError::Persistence("fork operation was not prepared".to_owned())
            })?;
            if journal.child_session_id != result.child.session_id
                || journal.parent.session_id != result.parent_session_id
                || journal.at_turn != result.at_turn
            {
                return Err(HostError::SessionIdentityMismatch);
            }
            if let ForkJournalState::Completed { result: existing } = &journal.state {
                if existing.child != result.child || existing.outcome != result.outcome {
                    return Err(HostError::RequestConflict);
                }
                return Ok(Self::completed_fork_result(existing));
            }
            journal.state = ForkJournalState::Completed {
                result: Box::new(ForkJournalResult {
                    protocol_version: result.protocol_version,
                    command_ack_emitted_at: result.command_ack_emitted_at,
                    fork_event_emitted_at: result.fork_event_emitted_at,
                    acknowledged_session_id: result.acknowledged_session_id,
                    outcome: result.outcome,
                    parent_session_id: result.parent_session_id,
                    child: result.child,
                    at_turn: result.at_turn,
                }),
            };
            journal.updated_unix_ms = unix_millis();
            let committed = factory.transition_fork_journal_unlocked(&journal)?;
            factory.enforce_live_fork_limits_unlocked(true)?;
            let ForkJournalState::Completed { result } = committed.state else {
                return Err(HostError::Persistence(
                    "fork completion did not reach its durable phase".to_owned(),
                ));
            };
            Ok(Self::completed_fork_result(&result))
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn abandon_prepared_fork_operation(
        &self,
        key: &ForkOperationKey,
    ) -> Result<(), HostError> {
        let factory = self.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let Some(journal) = factory.load_fork_journal_unlocked(&key)? else {
                return Ok(());
            };
            if !matches!(journal.state, ForkJournalState::Prepared) {
                return Ok(());
            }
            remove_forked_session_storage(
                &factory.options.storage_root,
                &journal.canonical_workspace,
                &journal.child_session_id.0,
            )
            .map_err(|_| HostError::Persistence("partial fork cleanup failed".to_owned()))?;
            let path = factory
                .ensure_fork_journal_directory()?
                .join(format!("{}.json", journal.operation_id));
            fs::remove_file(path).map_err(|_| {
                HostError::Persistence("prepared fork journal cleanup failed".to_owned())
            })?;
            fs::File::open(factory.fork_journal_directory())
                .map_err(|_| {
                    HostError::Persistence("prepared fork directory is unavailable".to_owned())
                })?
                .sync_all()
                .map_err(|_| {
                    HostError::Persistence("prepared fork cleanup could not sync".to_owned())
                })
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
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

    async fn search_persisted_sessions(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<SessionDescriptor>, bool), HostError> {
        let factory = self.clone();
        let query = query.to_owned();
        tokio::time::timeout(
            QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || factory.search_sessions_blocking(&query, limit)),
        )
        .await
        .map_err(|_| HostError::Query("session search deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("session search worker failed".to_owned()))?
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
        factory_with_allowed_workspaces(root, vec![workspace.to_path_buf()])
    }

    fn factory_with_allowed_workspaces(
        root: &Path,
        allowed_workspaces: Vec<PathBuf>,
    ) -> CliSessionFactory {
        let storage_root = private_test_directory(&root.join("state"));
        CliSessionFactory::new(CliHostOptions {
            credentials_path: storage_root.join("credentials.json"),
            storage_root,
            config: Config::default(),
            allowed_workspaces,
            permission_mode: Some(PermissionMode::Strict),
            max_turns: 2,
            provider_mode: HostedProviderMode::DeterministicReplay {
                provider_name: "offline-host".to_owned(),
                scripts: Vec::new(),
            },
            dangerously_trust: false,
        })
        .expect("factory")
    }

    fn private_test_directory(path: &Path) -> PathBuf {
        fs::create_dir_all(path).expect("private test directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("private test directory permissions");
        }
        fs::canonicalize(path).expect("canonical private test directory")
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

    #[tokio::test]
    async fn hosted_add_dir_enforces_allowed_roots_before_generation_or_tool_access() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let allowed = root.path().join("allowed");
        let outside = root.path().join("outside");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&allowed).expect("allowed");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("OUTSIDE_CANARY.txt"), "outside").expect("outside canary");
        let workspace = fs::canonicalize(workspace).expect("canonical workspace");
        let allowed = fs::canonicalize(allowed).expect("canonical allowed");
        let outside = fs::canonicalize(outside).expect("canonical outside");
        let factory =
            factory_with_allowed_workspaces(root.path(), vec![workspace.clone(), allowed.clone()]);
        let session_id = SessionId("hosted-add-root-policy".to_owned());
        let hosted = factory
            .create(CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("create hosted session");
        let handle = hosted.handle();

        let denied = handle
            .send_message(format!("/add-dir {}", outside.display()))
            .await
            .expect_err("outside root must be denied");
        assert!(denied.to_string().contains("authorization policy"));
        let unchanged = handle.snapshot().await.expect("unchanged snapshot");
        assert_eq!(unchanged.workspace_generation, 0);
        assert_eq!(unchanged.workspace_roots.len(), 1);
        assert_eq!(
            factory
                .workspace_roots_for_session(&hosted.descriptor())
                .expect("host roots after denial"),
            vec![workspace.clone()]
        );
        assert!(
            factory
                .preview_workspace_file(&hosted.descriptor(), "@root/1/OUTSIDE_CANARY.txt", 1024,)
                .await
                .is_err(),
            "denied root must not become queryable through hosted tool paths"
        );

        let allowed_session_id = SessionId("hosted-add-root-allowed".to_owned());
        let allowed_hosted = factory
            .create(CreateSessionRequest {
                session_id: allowed_session_id,
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("create allowed-root session");
        let allowed_handle = allowed_hosted.handle();
        allowed_handle
            .send_message(format!("/add-dir {}", allowed.display()))
            .await
            .expect("configured allowed root");
        let changed = allowed_handle.snapshot().await.expect("changed snapshot");
        assert_eq!(changed.workspace_generation, 1);
        assert_eq!(changed.workspace_roots.len(), 2);
        assert_eq!(
            factory
                .workspace_roots_for_session(&allowed_hosted.descriptor())
                .expect("host roots after allowed add"),
            vec![workspace, allowed]
        );
        assert!(!outside.join("created-by-tool.txt").exists());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn production_factory_fork_composes_and_resumes_child() {
        use rw_core::{
            ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome, ForkOperationKey,
            PROTOCOL_VERSION, PreparedForkOperation, RequestId, TurnId,
            runtime_support::{FinishReason, ProviderEvent},
        };

        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let storage_root = private_test_directory(&root.path().join("state"));
        let factory = CliSessionFactory::new(CliHostOptions {
            credentials_path: storage_root.join("credentials.json"),
            storage_root: storage_root.clone(),
            config: Config::default(),
            allowed_workspaces: vec![workspace.clone()],
            permission_mode: Some(PermissionMode::Strict),
            max_turns: 2,
            provider_mode: HostedProviderMode::DeterministicReplay {
                provider_name: "fork-production-offline".to_owned(),
                scripts: vec![vec![
                    ProviderEvent::TextDelta {
                        text: "completed parent turn".to_owned(),
                    },
                    ProviderEvent::Finished {
                        reason: FinishReason::Stop,
                    },
                ]],
            },
            dangerously_trust: false,
        })
        .expect("factory");
        let parent_id = SessionId("production-fork-parent".to_owned());
        let driver = ClientId("production-driver".to_owned());
        let parent = factory
            .create(CreateSessionRequest {
                session_id: parent_id.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("parent composes");
        let mut events = parent.handle().subscribe();
        assert_eq!(
            parent
                .handle()
                .dispatch(ClientCommand::AttachSession {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.clone(),
                        request_id: RequestId("production-attach".to_owned()),
                    },
                    session_id: parent_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("attach dispatch"),
            CommandOutcome::Accepted
        );
        assert_eq!(
            parent
                .handle()
                .dispatch(ClientCommand::SendMessage {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.clone(),
                        request_id: RequestId("production-message".to_owned()),
                    },
                    session_id: parent_id.clone(),
                    content: "complete one durable turn".to_owned(),
                    attachments: Vec::new(),
                })
                .await
                .expect("parent message"),
            CommandOutcome::Accepted
        );
        loop {
            if matches!(
                events.recv().await.expect("parent event"),
                rw_core::EngineEvent::TurnFinished { .. }
            ) {
                break;
            }
        }
        let switched_model = ModelAlias("historical-parent-later-model".to_owned());
        assert_eq!(
            parent
                .handle()
                .dispatch(ClientCommand::SwitchModel {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.clone(),
                        request_id: RequestId("production-switch-after-boundary".to_owned()),
                    },
                    session_id: parent_id.clone(),
                    model: switched_model.clone(),
                })
                .await
                .expect("switch parent model after fork boundary"),
            CommandOutcome::Accepted
        );
        loop {
            if matches!(
                events.recv().await.expect("parent model event"),
                rw_core::EngineEvent::ModelChanged { ref model, .. } if *model == switched_model
            ) {
                break;
            }
        }
        let parent_path = storage_root
            .join("sessions")
            .join(&parent_id.0)
            .join("events.jsonl");
        let parent_before = fs::read(&parent_path).expect("parent bytes");
        let child_id = SessionId("production-fork-child".to_owned());
        let fork_turn = TurnId("1".to_owned());
        let fork_payload_hash = blake3::hash(
            &serde_json::to_vec(&(&parent_id, &Some(fork_turn.clone())))
                .expect("stable fork payload"),
        )
        .to_hex()
        .to_string();
        let operation_key = ForkOperationKey {
            operation_id: "production-fork-operation".to_owned(),
            client_id: driver.clone(),
            request_id: RequestId("production-fork".to_owned()),
            payload_hash: fork_payload_hash.clone(),
        };
        let fork_request = ForkSessionRequest {
            operation_key: operation_key.clone(),
            parent: SessionDescriptor {
                driver_client_id: Some(driver.clone()),
                model: switched_model,
                ..parent.descriptor()
            },
            child_session_id: child_id.clone(),
            at_turn: fork_turn.clone(),
            through_sequence: None,
            include_idle_tail: false,
            driver_client_id: driver.clone(),
        };
        factory
            .prepare_fork_operation(PreparedForkOperation {
                key: operation_key,
                request: fork_request.clone(),
            })
            .await
            .expect("prepare production fork");
        let child = factory
            .fork(fork_request)
            .await
            .expect("production fork composes");
        assert_eq!(child.descriptor().session_id, child_id);
        assert_eq!(child.descriptor().model, ModelAlias("fast".to_owned()));
        let snapshot = child.handle().snapshot().await.expect("child snapshot");
        assert_eq!(snapshot.completed_turns, 1);
        assert_eq!(snapshot.driver_client_id, Some(driver));
        assert_eq!(
            fs::read(parent_path).expect("parent after fork"),
            parent_before
        );
        assert!(
            rw_store::session::AccountingLedger::open(&storage_root)
                .expect("accounting ledger")
                .entries_for_session(&child.descriptor().session_id.0)
                .expect("child accounting")
                .is_empty()
        );
        assert!(
            storage_root
                .join("sessions")
                .join(&child_id.0)
                .join("metadata.json")
                .is_file()
        );

        let durable_key = ForkOperationKey {
            operation_id: "production-fork-operation".to_owned(),
            client_id: ClientId("production-driver".to_owned()),
            request_id: RequestId("production-fork".to_owned()),
            payload_hash: fork_payload_hash,
        };
        let mut journal = factory
            .load_fork_journal(&durable_key)
            .expect("load storage-committed journal")
            .expect("journal exists");
        assert!(matches!(journal.state, ForkJournalState::StorageCommitted));
        // Simulate a kill after metadata fsync but before the phase rewrite.
        journal.state = ForkJournalState::Prepared;
        factory
            .force_replace_fork_journal_for_test(&journal)
            .expect("simulate prepared crash state");
        let restart_options = (*factory.options).clone();
        drop(events);
        drop(child);
        drop(parent);
        drop(factory);
        tokio::task::yield_now().await;
        let restarted =
            Arc::new(CliSessionFactory::new(restart_options.clone()).expect("restart recovery"));
        let promoted = restarted
            .load_fork_journal(&durable_key)
            .expect("load promoted journal")
            .expect("promoted journal exists");
        assert!(matches!(promoted.state, ForkJournalState::StorageCommitted));
        assert_eq!(
            CliSessionFactory::journal_operation(&promoted)
                .request
                .child_session_id,
            child_id
        );
        let restarted_client_key = ForkOperationKey {
            operation_id: durable_key.operation_id.clone(),
            client_id: ClientId("replacement-driver".to_owned()),
            request_id: RequestId("retry-after-process-restart".to_owned()),
            payload_hash: durable_key.payload_hash.clone(),
        };
        let host = rw_core::EngineHost::new(
            rw_core::EngineHostConfig::default(),
            restarted.clone(),
            restarted.clone(),
        )
        .expect("restart host");
        let replacement = rw_core::BoundClient {
            client_id: restarted_client_key.client_id.clone(),
        };
        let mut replacement_events = host
            .subscribe(replacement.clone(), None, None)
            .await
            .expect("replacement event stream");
        assert_eq!(
            host.dispatch(
                replacement,
                rw_core::ClientCommand::Fork {
                    meta: rw_core::CommandMeta {
                        protocol_version: rw_core::PROTOCOL_VERSION,
                        client_id: ClientId("wire-spoof-is-replaced".to_owned()),
                        request_id: restarted_client_key.request_id.clone(),
                    },
                    session_id: parent_id.clone(),
                    at_turn: Some(fork_turn.clone()),
                    operation_id: Some(restarted_client_key.operation_id.clone()),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let replayed_child = loop {
            if let EngineEvent::SessionForked { child, meta, .. } = replacement_events
                .recv()
                .await
                .expect("replayed fork event")
                .expect("replayed fork result")
            {
                assert_eq!(meta.client_id, restarted_client_key.client_id);
                assert_eq!(meta.request_id, restarted_client_key.request_id);
                break child;
            }
        };
        assert_eq!(replayed_child.session_id, child_id);
        assert_eq!(
            replayed_child.driver_client_id,
            Some(restarted_client_key.client_id.clone())
        );
        let completion = match restarted
            .load_fork_operation(&restarted_client_key)
            .await
            .expect("load completion after stable retry")
        {
            ForkOperationState::Completed(completion) => completion,
            state => panic!("stable retry did not complete: {state:?}"),
        };
        assert_eq!(completion.child, replayed_child);
        let mut racing_completion = completion.clone();
        racing_completion.command_ack_emitted_at = "2026-07-11T00:00:02.000Z".to_owned();
        racing_completion.fork_event_emitted_at = "2026-07-11T00:00:03.000Z".to_owned();
        assert_eq!(
            restarted
                .complete_fork_operation(&restarted_client_key, &racing_completion)
                .await
                .expect("racing completion returns authoritative result"),
            completion
        );
        journal.state = ForkJournalState::StorageCommitted;
        let monotonic = restarted
            .transition_fork_journal_for_test(&journal)
            .expect("stale transition is read-modify-write guarded");
        assert!(matches!(
            monotonic.state,
            ForkJournalState::Completed { .. }
        ));
        drop(replacement_events);
        drop(host);
        drop(restarted);
        tokio::task::yield_now().await;
        let mut grown_child = SessionEventLog::open(&storage_root, &child_id.0)
            .expect("open completed child for post-fork growth");
        let target_events = crate::history::MAX_HISTORY_EVENTS + 1;
        while usize::try_from(grown_child.next_sequence()).expect("child sequence") < target_events
        {
            let start = grown_child.next_sequence();
            let remaining =
                target_events.saturating_sub(usize::try_from(start).expect("child sequence index"));
            let count = remaining.min(10_000);
            let batch = (0..count)
                .map(|offset| {
                    let sequence = start + u64::try_from(offset).expect("batch offset");
                    EngineEvent::ModeChanged {
                        meta: rw_core::EventMeta {
                            protocol_version: rw_core::PROTOCOL_VERSION,
                            session_id: child_id.clone(),
                            sequence_id: rw_core::SequenceId(sequence),
                            emitted_at: "2026-07-11T00:00:04Z".to_owned(),
                            caused_by: None,
                        },
                        mode: rw_core::ModeId("execute".to_owned()),
                    }
                })
                .collect::<Vec<_>>();
            grown_child
                .append_batch(batch)
                .expect("append valid post-fork child events");
        }
        drop(grown_child);
        assert!(matches!(
            SessionEventLog::load_existing_bounded::<EngineEvent>(
                &storage_root,
                &child_id.0,
                crate::history::MAX_HISTORY_BYTES,
                crate::history::MAX_HISTORY_EVENTS,
            ),
            Err(rw_store::session::SessionStoreError::EventCountTooLarge { .. })
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(
                storage_root
                    .join("sessions")
                    .join(&child_id.0)
                    .join("events.jsonl"),
                fs::Permissions::from_mode(0o000),
            )
            .expect("install completed-child no-read canary");
            fs::set_permissions(
                crate::runtime::checkpoint_root(&storage_root, &workspace, &child_id.0)
                    .join("workspace-roots.json"),
                fs::Permissions::from_mode(0o000),
            )
            .expect("install completed-child root-journal no-read canary");
        }
        let reloaded =
            Arc::new(CliSessionFactory::new(restart_options).expect("completed restart"));
        assert_eq!(
            reloaded
                .load_fork_operation(&durable_key)
                .await
                .expect("reload completed result"),
            ForkOperationState::Completed(completion.clone())
        );
        let second_restart_key = ForkOperationKey {
            operation_id: durable_key.operation_id.clone(),
            client_id: ClientId("second-replacement-driver".to_owned()),
            request_id: RequestId("retry-after-second-restart".to_owned()),
            payload_hash: durable_key.payload_hash.clone(),
        };
        assert_eq!(
            reloaded
                .load_fork_operation(&second_restart_key)
                .await
                .expect("stable operation id survives client and request replacement"),
            ForkOperationState::Completed(completion.clone())
        );
        let host = rw_core::EngineHost::new(
            rw_core::EngineHostConfig::default(),
            reloaded.clone(),
            reloaded.clone(),
        )
        .expect("restart host");
        let replacement = rw_core::BoundClient {
            client_id: second_restart_key.client_id.clone(),
        };
        let mut replacement_events = host
            .subscribe(replacement.clone(), None, None)
            .await
            .expect("replacement event stream");
        assert_eq!(
            host.dispatch(
                replacement,
                rw_core::ClientCommand::Fork {
                    meta: rw_core::CommandMeta {
                        protocol_version: rw_core::PROTOCOL_VERSION,
                        client_id: ClientId("wire-spoof-is-replaced".to_owned()),
                        request_id: second_restart_key.request_id.clone(),
                    },
                    session_id: completion.parent_session_id.clone(),
                    at_turn: Some(completion.at_turn.clone()),
                    operation_id: Some(second_restart_key.operation_id.clone()),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let replayed_child = loop {
            if let EngineEvent::SessionForked { child, meta, .. } = replacement_events
                .recv()
                .await
                .expect("replayed fork event")
                .expect("replayed fork result")
            {
                assert_eq!(meta.client_id, second_restart_key.client_id);
                assert_eq!(meta.request_id, second_restart_key.request_id);
                break child;
            }
        };
        assert_eq!(replayed_child.session_id, completion.child.session_id);
        let conflict = ForkOperationKey {
            payload_hash: "b".repeat(64),
            ..durable_key
        };
        assert_eq!(
            reloaded
                .load_fork_operation(&conflict)
                .await
                .expect_err("payload conflict"),
            HostError::RequestConflict
        );
    }

    #[tokio::test]
    async fn prepared_fork_recovery_cleans_partial_trees_and_keeps_child_identity() {
        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        let parent = factory
            .create(CreateSessionRequest {
                session_id: SessionId("journal-parent".to_owned()),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("parent");
        let key = ForkOperationKey {
            operation_id: "journal-operation".to_owned(),
            client_id: rw_core::ClientId("journal-client".to_owned()),
            request_id: rw_core::RequestId("journal-request".to_owned()),
            payload_hash: "c".repeat(64),
        };
        let child = SessionId("journal-child".to_owned());
        let operation = PreparedForkOperation {
            key: key.clone(),
            request: ForkSessionRequest {
                operation_key: key.clone(),
                parent: parent.descriptor(),
                child_session_id: child.clone(),
                at_turn: rw_core::TurnId("0".to_owned()),
                through_sequence: None,
                include_idle_tail: false,
                driver_client_id: key.client_id.clone(),
            },
        };
        factory
            .prepare_fork_operation(operation.clone())
            .await
            .expect("prepare");
        let session_tree = factory.options.storage_root.join("sessions").join(&child.0);
        fs::create_dir_all(&session_tree).expect("partial session tree");
        fs::write(session_tree.join("events.jsonl"), b"partial").expect("partial log");
        fs::write(session_tree.join(".metadata-crash.tmp"), br#"{"version":1"#)
            .expect("unpublished metadata temporary");
        let digest = blake3::hash(workspace.as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string();
        let checkpoint_tree = factory
            .options
            .storage_root
            .join("workspaces")
            .join(digest)
            .join("sessions")
            .join(&child.0);
        fs::create_dir_all(&checkpoint_tree).expect("partial checkpoint tree");
        fs::write(checkpoint_tree.join("partial"), b"partial").expect("partial checkpoint");
        let restarted = CliSessionFactory::new((*factory.options).clone()).expect("recover");
        assert!(!session_tree.exists());
        assert!(!checkpoint_tree.exists());
        assert_eq!(
            restarted
                .load_fork_operation(&key)
                .await
                .expect("load prepared"),
            ForkOperationState::Pending(operation)
        );
    }

    #[tokio::test]
    async fn session_capacity_rejection_abandons_prepared_fork_journal() {
        use rw_core::{
            BoundClient, ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome,
            EngineHost, EngineHostConfig, PROTOCOL_VERSION, RequestId,
        };

        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = Arc::new(factory(root.path(), &workspace));
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 64,
            },
            factory.clone(),
            factory.clone(),
        )
        .expect("host");
        let parent = SessionId("capacity-parent".to_owned());
        host.prepare_session(
            CreateSessionRequest {
                session_id: parent.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            },
            false,
        )
        .await
        .expect("parent");
        let driver = BoundClient {
            client_id: ClientId("capacity-driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::AttachSession {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.client_id.clone(),
                        request_id: RequestId("capacity-attach".to_owned()),
                    },
                    session_id: parent.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let outcome = host
            .dispatch(
                driver.clone(),
                ClientCommand::Fork {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.client_id,
                        request_id: RequestId("capacity-fork".to_owned()),
                    },
                    session_id: parent,
                    at_turn: None,
                    operation_id: Some("capacity-fork-operation".to_owned()),
                },
            )
            .await;
        assert!(matches!(
            outcome,
            CommandOutcome::Rejected { error } if error.code == "session_capacity"
        ));
        assert!(
            fs::read_dir(factory.fork_journal_directory())
                .expect("journal directory")
                .all(|entry| entry
                    .expect("journal entry")
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("json"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn fork_journal_cross_process_lock_helper() {
        let Ok(root) = std::env::var("RW_TEST_FORK_LOCK_ROOT") else {
            return;
        };
        let workspace =
            PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_WORKSPACE").expect("helper workspace"));
        let ready = PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_READY").expect("helper ready"));
        let release =
            PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_RELEASE").expect("helper release"));
        let factory = factory(Path::new(&root), &workspace);
        let _lock = factory
            .acquire_fork_journal_lock()
            .expect("helper acquires lock");
        fs::write(ready, b"ready").expect("helper ready marker");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !release.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(release.exists(), "parent releases helper lock");
    }

    #[test]
    #[cfg(unix)]
    fn fork_recovery_waits_for_cross_process_journal_lock() {
        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        let options = (*factory.options).clone();
        let ready = root.path().join("lock-ready");
        let release = root.path().join("lock-release");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .arg("--exact")
            .arg("host_runtime::tests::fork_journal_cross_process_lock_helper")
            .arg("--nocapture")
            .env("RW_TEST_FORK_LOCK_ROOT", root.path())
            .env("RW_TEST_FORK_LOCK_WORKSPACE", &workspace)
            .env("RW_TEST_FORK_LOCK_READY", &ready)
            .env("RW_TEST_FORK_LOCK_RELEASE", &release)
            .spawn()
            .expect("lock helper");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "helper acquired cross-process lock");

        let (send, receive) = std::sync::mpsc::channel();
        let recovery = std::thread::spawn(move || {
            let result = CliSessionFactory::new(options);
            send.send(result.is_ok()).expect("recovery result");
        });
        assert!(
            receive.recv_timeout(Duration::from_millis(100)).is_err(),
            "recovery must wait while another process owns the journal lock"
        );
        fs::write(&release, b"release").expect("release marker");
        assert!(child.wait().expect("helper exit").success());
        assert!(
            receive
                .recv_timeout(Duration::from_secs(5))
                .expect("recovery completes")
        );
        recovery.join().expect("recovery thread");
    }

    #[test]
    #[cfg(unix)]
    fn fork_journal_rejects_unexpected_symlink_hardlink_and_oversized_entries() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        let options = (*factory.options).clone();
        let directory = factory.fork_journal_directory();

        let unpublished = directory.join(".fork-crash.tmp");
        fs::write(&unpublished, br#"{"version":1"#).expect("unpublished journal temporary");
        fs::set_permissions(&unpublished, fs::Permissions::from_mode(0o600))
            .expect("private unpublished journal");
        CliSessionFactory::new(options.clone()).expect("orphan temporary is recoverable");
        assert!(!unpublished.exists());

        fs::write(directory.join("unexpected"), b"x").expect("unexpected entry");
        assert!(CliSessionFactory::new(options.clone()).is_err());
        fs::remove_file(directory.join("unexpected")).expect("remove unexpected");

        let outside = root.path().join("outside");
        fs::write(&outside, b"{}").expect("outside");
        symlink(&outside, directory.join(format!("{}.json", "a".repeat(64)))).expect("symlink");
        assert!(CliSessionFactory::new(options.clone()).is_err());
        fs::remove_file(directory.join(format!("{}.json", "a".repeat(64))))
            .expect("remove symlink");

        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).expect("private source");
        fs::hard_link(&outside, directory.join(format!("{}.json", "b".repeat(64))))
            .expect("hardlink");
        assert!(CliSessionFactory::new(options.clone()).is_err());
        fs::remove_file(directory.join(format!("{}.json", "b".repeat(64))))
            .expect("remove hardlink");

        let oversized = directory.join(format!("{}.json", "c".repeat(64)));
        fs::write(&oversized, vec![b'x'; MAX_FORK_JOURNAL_BYTES + 1]).expect("oversized");
        fs::set_permissions(&oversized, fs::Permissions::from_mode(0o600)).expect("private file");
        assert!(CliSessionFactory::new(options).is_err());
    }
}
