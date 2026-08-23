#[cfg(not(unix))]
use std::fs;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(not(unix))]
use std::path::PathBuf;
use std::{
    io::{Read, Write},
    path::Path,
};

use async_trait::async_trait;
use rw_core::{OrchestrationError, SubagentMetadataStore, SubagentRecoveryRecord};
use rw_types::{SessionId, SubagentId};
use serde::{Deserialize, Serialize};

const VERSION: u16 = 1;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_RECORDS: usize = 256;
#[cfg(unix)]
const METADATA_DIRECTORY: &str = "subagents-v1";

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    version: u16,
    record: SubagentRecoveryRecord,
}

#[derive(Debug)]
pub(crate) struct PrivateSubagentMetadataStore {
    #[cfg(not(unix))]
    root: PathBuf,
    #[cfg(unix)]
    storage_root: OwnedFd,
    #[cfg(unix)]
    root: OwnedFd,
    #[cfg(unix)]
    root_identity: PinnedDirectoryIdentity,
}

impl PrivateSubagentMetadataStore {
    pub(crate) fn open(storage_root: &Path) -> Result<Self, OrchestrationError> {
        #[cfg(unix)]
        return open_unix(storage_root);
        #[cfg(not(unix))]
        {
            let root = storage_root.join("subagents-v1");
            create_private_dir(&root)?;
            Ok(Self { root })
        }
    }

    pub(crate) fn load_parent(
        &self,
        parent: &SessionId,
    ) -> Result<Vec<SubagentRecoveryRecord>, OrchestrationError> {
        #[cfg(unix)]
        {
            self.validate_root_namespace()?;
            load_parent_unix(&self.root, parent)
        }
        #[cfg(not(unix))]
        {
            validate_session_id(parent)?;
            let directory = self.root.join(&parent.0);
            match fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(session_error(
                        "subagent metadata parent path is not a directory",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(io_error("inspect subagent metadata directory", error)),
            }
            let mut paths = fs::read_dir(&directory)
                .map_err(|error| io_error("read subagent metadata directory", error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| io_error("read subagent metadata entry", error))?;
            paths.sort_by_key(std::fs::DirEntry::file_name);
            if paths.len() > MAX_RECORDS {
                return Err(session_error("subagent metadata record limit exceeded"));
            }
            let mut records = Vec::with_capacity(paths.len());
            for entry in paths {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    return Err(session_error(
                        "unexpected file in subagent metadata directory",
                    ));
                }
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|error| io_error("inspect subagent metadata record", error))?;
                if !metadata.file_type().is_file() || metadata.len() > MAX_RECORD_BYTES {
                    return Err(session_error(
                        "subagent metadata record is unsafe or oversized",
                    ));
                }
                let mut bytes = Vec::with_capacity(metadata.len() as usize);
                OpenOptions::new()
                    .read(true)
                    .open(&path)
                    .and_then(|mut file| file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes))
                    .map_err(|error| io_error("read subagent metadata record", error))?;
                if bytes.len() as u64 > MAX_RECORD_BYTES {
                    return Err(session_error("subagent metadata record is oversized"));
                }
                let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|error| {
                    session_error(format!("subagent metadata is corrupt: {error}"))
                })?;
                if envelope.version != VERSION || &envelope.record.parent_session_id != parent {
                    return Err(session_error(
                        "subagent metadata identity or version mismatch",
                    ));
                }
                validate_component(&envelope.record.handle.subagent_id.0)?;
                validate_session_id(&envelope.record.handle.session_id)?;
                let expected = format!("{}.json", envelope.record.handle.subagent_id.0);
                if path.file_name().and_then(|value| value.to_str()) != Some(expected.as_str()) {
                    return Err(session_error(
                        "subagent metadata filename does not match its identity",
                    ));
                }
                records.push(envelope.record);
            }
            Ok(records)
        }
    }

    #[cfg(not(unix))]
    fn parent_dir(&self, parent: &SessionId) -> Result<PathBuf, OrchestrationError> {
        validate_session_id(parent)?;
        let path = self.root.join(&parent.0);
        create_private_dir(&path)?;
        Ok(path)
    }
}

#[async_trait]
impl SubagentMetadataStore for PrivateSubagentMetadataStore {
    async fn save(&self, record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
        #[cfg(unix)]
        {
            self.validate_root_namespace()?;
            save_unix(&self.root, record)
        }
        #[cfg(not(unix))]
        {
            validate_component(&record.handle.subagent_id.0)?;
            validate_session_id(&record.handle.session_id)?;
            let directory = self.parent_dir(&record.parent_session_id)?;
            let destination = directory.join(format!("{}.json", record.handle.subagent_id.0));
            let temporary = directory.join(format!(
                ".{}.{}.tmp",
                record.handle.subagent_id.0,
                std::process::id()
            ));
            let bytes = serde_json::to_vec(&Envelope {
                version: VERSION,
                record,
            })
            .map_err(|error| {
                session_error(format!("subagent metadata could not encode: {error}"))
            })?;
            if bytes.len() as u64 > MAX_RECORD_BYTES {
                return Err(session_error("subagent metadata record is oversized"));
            }
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .map_err(|error| io_error("create temporary subagent metadata", error))?;
            let result = (|| {
                file.write_all(&bytes)?;
                file.sync_all()?;
                fs::rename(&temporary, &destination)?;
                FileSync::sync_directory(&directory)?;
                Ok::<_, std::io::Error>(())
            })();
            if let Err(error) = result {
                let _ = fs::remove_file(&temporary);
                return Err(io_error("persist subagent metadata", error));
            }
            Ok(())
        }
    }

    async fn remove(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        #[cfg(unix)]
        {
            self.validate_root_namespace()?;
            remove_unix(&self.root, parent_session_id, subagent_id)
        }
        #[cfg(not(unix))]
        {
            validate_session_id(parent_session_id)?;
            validate_component(&subagent_id.0)?;
            let directory = self.root.join(&parent_session_id.0);
            let path = directory.join(format!("{}.json", subagent_id.0));
            match fs::remove_file(path) {
                Ok(()) => FileSync::sync_directory(&directory)
                    .map_err(|error| io_error("sync subagent metadata directory", error)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error("remove subagent metadata", error)),
            }
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PinnedDirectoryIdentity {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
}

#[cfg(unix)]
fn pinned_directory_identity(
    descriptor: &OwnedFd,
    label: &str,
) -> Result<PinnedDirectoryIdentity, OrchestrationError> {
    use rustix::fs::{FileType, Mode};
    let stat = rustix::fs::fstat(descriptor)
        .map_err(|error| io_error("inspect pinned metadata directory", error.into()))?;
    let mode = crate::rustix_mode_bits(Mode::from_raw_mode(stat.st_mode).as_raw_mode()) & 0o777;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || mode != 0o700
    {
        return Err(session_error(format!(
            "{label} must be owned by the current user with mode 0700"
        )));
    }
    Ok(PinnedDirectoryIdentity {
        device: crate::rustix_device_id(stat.st_dev)
            .ok_or_else(|| session_error("metadata directory device identity overflow"))?,
        inode: stat.st_ino,
        uid: stat.st_uid,
        mode,
    })
}

#[cfg(unix)]
fn open_storage_root(path: &Path) -> Result<OwnedFd, OrchestrationError> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::open(path, flags, Mode::empty())
        .map_err(|error| io_error("open metadata storage root", error.into()))
}

#[cfg(unix)]
fn open_unix(storage_root: &Path) -> Result<PrivateSubagentMetadataStore, OrchestrationError> {
    use rustix::fs::{Mode, OFlags};
    let storage_root = open_storage_root(storage_root)?;
    let created = match rustix::fs::mkdirat(
        &storage_root,
        METADATA_DIRECTORY,
        Mode::from_raw_mode(0o700),
    ) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(error) => {
            return Err(io_error(
                "create dedicated subagent metadata directory",
                error.into(),
            ));
        }
    };
    let root = rustix::fs::openat(
        &storage_root,
        METADATA_DIRECTORY,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io_error("open dedicated subagent metadata directory", error.into()))?;
    let stat = rustix::fs::fstat(&root).map_err(|error| {
        io_error(
            "inspect dedicated subagent metadata directory",
            error.into(),
        )
    })?;
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(session_error(
            "subagent metadata root must be owned by the current user",
        ));
    }
    rustix::fs::fchmod(&root, Mode::from_raw_mode(0o700))
        .map_err(|error| io_error("secure dedicated subagent metadata directory", error.into()))?;
    if created {
        rustix::fs::fsync(&storage_root)
            .map_err(|error| io_error("sync metadata storage root", error.into()))?;
    }
    let root_identity = pinned_directory_identity(&root, "subagent metadata root")?;
    Ok(PrivateSubagentMetadataStore {
        storage_root,
        root,
        root_identity,
    })
}

#[cfg(unix)]
impl PrivateSubagentMetadataStore {
    fn validate_root_namespace(&self) -> Result<(), OrchestrationError> {
        use rustix::fs::{Mode, OFlags};
        let retained = pinned_directory_identity(&self.root, "subagent metadata root")?;
        if retained != self.root_identity {
            return Err(session_error(
                "pinned subagent metadata root identity changed",
            ));
        }
        let current = rustix::fs::openat(
            &self.storage_root,
            METADATA_DIRECTORY,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| io_error("reopen pinned subagent metadata root", error.into()))?;
        if pinned_directory_identity(&current, "subagent metadata root")? != self.root_identity {
            return Err(session_error(
                "subagent metadata root was replaced after opening",
            ));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn open_parent(
    root: &OwnedFd,
    parent: &SessionId,
    create: bool,
) -> Result<Option<std::os::fd::OwnedFd>, OrchestrationError> {
    use rustix::fs::{Mode, OFlags};
    validate_session_id(parent)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(root, parent.0.as_str(), flags, Mode::empty()) {
        Ok(directory) => {
            validate_private_directory(&directory, "subagent metadata parent")?;
            Ok(Some(directory))
        }
        Err(rustix::io::Errno::NOENT) if !create => Ok(None),
        Err(rustix::io::Errno::NOENT) => {
            match rustix::fs::mkdirat(root, parent.0.as_str(), Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => {
                    return Err(io_error("create subagent metadata parent", error.into()));
                }
            }
            rustix::fs::fsync(root)
                .map_err(|error| io_error("sync subagent metadata root", error.into()))?;
            let directory = rustix::fs::openat(root, parent.0.as_str(), flags, Mode::empty())
                .map_err(|error| io_error("open subagent metadata parent", error.into()))?;
            validate_private_directory(&directory, "subagent metadata parent")?;
            Ok(Some(directory))
        }
        Err(error) => Err(io_error("open subagent metadata parent", error.into())),
    }
}

#[cfg(unix)]
fn validate_private_directory(
    descriptor: &std::os::fd::OwnedFd,
    label: &str,
) -> Result<(), OrchestrationError> {
    use rustix::fs::{FileType, Mode};
    let stat = rustix::fs::fstat(descriptor)
        .map_err(|error| io_error("inspect private metadata directory", error.into()))?;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || (Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o777) != 0o700
    {
        return Err(session_error(format!(
            "{label} must be owned by the current user with mode 0700 or stricter"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn load_parent_unix(
    root: &OwnedFd,
    parent: &SessionId,
) -> Result<Vec<SubagentRecoveryRecord>, OrchestrationError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    let Some(directory) = open_parent(root, parent, false)? else {
        return Ok(Vec::new());
    };
    let mut entries = rustix::fs::Dir::read_from(&directory)
        .map_err(|error| io_error("read subagent metadata directory", error.into()))?;
    let mut names = Vec::new();
    while let Some(entry) = entries.read() {
        let entry =
            entry.map_err(|error| io_error("read subagent metadata entry", error.into()))?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| session_error("subagent metadata filename is not UTF-8"))?;
        if matches!(name, "." | "..") || is_owned_temp(name) {
            continue;
        }
        if Path::new(name).extension() != Some(std::ffi::OsStr::new("json")) {
            return Err(session_error(
                "unexpected file in subagent metadata directory",
            ));
        }
        names.push(name.to_owned());
    }
    names.sort();
    if names.len() > MAX_RECORDS {
        return Err(session_error("subagent metadata record limit exceeded"));
    }
    let mut records = Vec::with_capacity(names.len());
    for name in names {
        let stat = rustix::fs::statat(&directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| io_error("inspect subagent metadata record", error.into()))?;
        let stat_size = u64::try_from(stat.st_size)
            .map_err(|_| session_error("subagent metadata record has invalid size"))?;
        if !FileType::from_raw_mode(stat.st_mode).is_file() || stat_size > MAX_RECORD_BYTES {
            return Err(session_error(
                "subagent metadata record is unsafe or oversized",
            ));
        }
        let descriptor = rustix::fs::openat(
            &directory,
            name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| io_error("open subagent metadata record", error.into()))?;
        let opened = rustix::fs::fstat(&descriptor)
            .map_err(|error| io_error("inspect opened subagent metadata record", error.into()))?;
        let opened_size = u64::try_from(opened.st_size)
            .map_err(|_| session_error("opened subagent metadata record has invalid size"))?;
        if !FileType::from_raw_mode(opened.st_mode).is_file()
            || opened_size > MAX_RECORD_BYTES
            || opened.st_dev != stat.st_dev
            || opened.st_ino != stat.st_ino
            || opened.st_uid != rustix::process::geteuid().as_raw()
            || opened.st_nlink != 1
            || (Mode::from_raw_mode(opened.st_mode).as_raw_mode() & 0o777) != 0o600
        {
            return Err(session_error(
                "subagent metadata record changed while opening",
            ));
        }
        let capacity = usize::try_from(opened_size)
            .map_err(|_| session_error("subagent metadata record size cannot be represented"))?;
        let mut bytes = Vec::with_capacity(capacity);
        std::fs::File::from(descriptor)
            .take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| io_error("read subagent metadata record", error))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(session_error("subagent metadata record is oversized"));
        }
        let envelope: Envelope = serde_json::from_slice(&bytes)
            .map_err(|error| session_error(format!("subagent metadata is corrupt: {error}")))?;
        if envelope.version != VERSION || &envelope.record.parent_session_id != parent {
            return Err(session_error(
                "subagent metadata identity or version mismatch",
            ));
        }
        validate_component(&envelope.record.handle.subagent_id.0)?;
        validate_session_id(&envelope.record.handle.session_id)?;
        if name != format!("{}.json", envelope.record.handle.subagent_id.0) {
            return Err(session_error(
                "subagent metadata filename does not match its identity",
            ));
        }
        records.push(envelope.record);
    }
    Ok(records)
}

#[cfg(unix)]
fn save_unix(root: &OwnedFd, record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    validate_component(&record.handle.subagent_id.0)?;
    validate_session_id(&record.handle.session_id)?;
    let directory = open_parent(root, &record.parent_session_id, true)?
        .ok_or_else(|| session_error("subagent metadata parent disappeared"))?;
    let destination = format!("{}.json", record.handle.subagent_id.0);
    let bytes = serde_json::to_vec(&Envelope {
        version: VERSION,
        record,
    })
    .map_err(|error| session_error(format!("subagent metadata could not encode: {error}")))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(session_error("subagent metadata record is oversized"));
    }
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| session_error("subagent metadata temp-name entropy failed"))?;
    let temporary = format!(".rw-subagent-{}.tmp", hex(&random));
    let descriptor = rustix::fs::openat(
        &directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| io_error("create subagent metadata temporary", error.into()))?;
    let mut file = std::fs::File::from(descriptor);
    let result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        rustix::fs::renameat(
            &directory,
            temporary.as_str(),
            &directory,
            destination.as_str(),
        )
        .map_err(std::io::Error::from)?;
        rustix::fs::fsync(&directory).map_err(std::io::Error::from)
    })();
    if let Err(error) = result {
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), AtFlags::empty());
        return Err(io_error("persist subagent metadata", error));
    }
    Ok(())
}

#[cfg(unix)]
fn remove_unix(
    root: &OwnedFd,
    parent: &SessionId,
    subagent: &SubagentId,
) -> Result<(), OrchestrationError> {
    use rustix::fs::AtFlags;
    validate_component(&subagent.0)?;
    let Some(directory) = open_parent(root, parent, false)? else {
        return Ok(());
    };
    let name = format!("{}.json", subagent.0);
    match rustix::fs::unlinkat(&directory, name.as_str(), AtFlags::empty()) {
        Ok(()) => rustix::fs::fsync(&directory)
            .map_err(|error| io_error("sync subagent metadata directory", error.into())),
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(io_error("remove subagent metadata", error.into())),
    }
}

#[cfg(unix)]
fn is_owned_temp(name: &str) -> bool {
    let Some(hex) = name
        .strip_prefix(".rw-subagent-")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(unix)]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut value, "{byte:02x}");
    }
    value
}

#[cfg(not(unix))]
struct FileSync;

#[cfg(not(unix))]
impl FileSync {
    fn sync_directory(path: &Path) -> std::io::Result<()> {
        std::fs::File::open(path)?.sync_all()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::items_after_test_module)]
mod tests {
    use std::sync::Arc;

    use rw_core::{
        SubagentHandle, SubagentMetadataStore as _, SubagentRecoveryPhase, SubagentRecoveryPolicy,
        SubagentRecoveryRecord,
    };
    use rw_tools::CapabilityManifest;
    use rw_types::{SessionMode, SubagentId, SubagentIsolation};
    use tempfile::TempDir;

    use super::PrivateSubagentMetadataStore;

    fn record() -> SubagentRecoveryRecord {
        SubagentRecoveryRecord {
            parent_session_id: rw_types::SessionId("parent".to_owned()),
            handle: SubagentHandle {
                subagent_id: SubagentId("child".to_owned()),
                session_id: rw_types::SessionId("child-session".to_owned()),
            },
            task: "fixture task".to_owned(),
            agent: "fixture agent".to_owned(),
            depth: 1,
            workspace_root: std::path::PathBuf::from("/private/worktree"),
            isolation: SubagentIsolation::Worktree,
            worktree: None,
            capabilities: CapabilityManifest::default(),
            tool_names: vec!["read".to_owned()],
            policy: SubagentRecoveryPolicy {
                model_alias: "fast".to_owned(),
                system_prompt: None,
                permission_mode: SessionMode::Execute,
                max_turns: 4,
            },
            phase: SubagentRecoveryPhase::Active,
        }
    }

    #[tokio::test]
    async fn atomic_store_round_trips_and_ignores_valid_crash_temp() {
        let root = TempDir::new().expect("root");
        let store = Arc::new(PrivateSubagentMetadataStore::open(root.path()).expect("store"));
        store.save(record()).await.expect("save");
        let directory = root.path().join("subagents-v1/parent");
        std::fs::write(
            directory.join(".rw-subagent-00000000000000000000000000000000.tmp"),
            b"partial",
        )
        .expect("stale temp");
        let loaded = store
            .load_parent(&rw_types::SessionId("parent".to_owned()))
            .expect("load");
        assert_eq!(loaded, vec![record()]);
    }

    #[cfg(unix)]
    #[test]
    fn parent_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("root");
        let outside = TempDir::new().expect("outside");
        let store = PrivateSubagentMetadataStore::open(root.path()).expect("store");
        symlink(outside.path(), root.path().join("subagents-v1/parent")).expect("symlink");
        assert!(
            store
                .load_parent(&rw_types::SessionId("parent".to_owned()))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn record_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("root");
        let outside = TempDir::new().expect("outside");
        let store = PrivateSubagentMetadataStore::open(root.path()).expect("store");
        let directory = root.path().join("subagents-v1/parent");
        std::fs::create_dir(&directory).expect("parent");
        std::fs::write(outside.path().join("record"), b"{}").expect("outside record");
        symlink(outside.path().join("record"), directory.join("child.json")).expect("symlink");
        assert!(
            store
                .load_parent(&rw_types::SessionId("parent".to_owned()))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hardlinked_or_public_record_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = TempDir::new().expect("root");
        let store = PrivateSubagentMetadataStore::open(root.path()).expect("store");
        store.save(record()).await.expect("save");
        let record_path = root.path().join("subagents-v1/parent/child.json");
        std::fs::hard_link(&record_path, root.path().join("leaked-record")).expect("hard link");
        assert!(
            store
                .load_parent(&rw_types::SessionId("parent".to_owned()))
                .is_err()
        );
        std::fs::remove_file(root.path().join("leaked-record")).expect("remove hard link");
        std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o644))
            .expect("public mode");
        assert!(
            store
                .load_parent(&rw_types::SessionId("parent".to_owned()))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn dedicated_root_preserves_caller_mode_and_rejects_namespace_replacement() {
        use std::os::unix::fs::PermissionsExt as _;

        let storage = TempDir::new().expect("storage");
        std::fs::set_permissions(storage.path(), std::fs::Permissions::from_mode(0o755))
            .expect("caller mode");
        let store = PrivateSubagentMetadataStore::open(storage.path()).expect("store");
        assert_eq!(
            std::fs::metadata(storage.path())
                .expect("storage metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "opening metadata must not chmod the caller storage root"
        );
        let dedicated = storage.path().join("subagents-v1");
        assert_eq!(
            std::fs::metadata(&dedicated)
                .expect("dedicated metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let parked = storage.path().join("subagents-v1-parked");
        std::fs::rename(&dedicated, &parked).expect("park pinned root");
        std::fs::create_dir(&dedicated).expect("replacement root");
        std::fs::set_permissions(&dedicated, std::fs::Permissions::from_mode(0o700))
            .expect("replacement mode");
        assert!(
            store
                .load_parent(&rw_types::SessionId("parent".to_owned()))
                .is_err(),
            "same-owner same-mode replacement must fail identity validation"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn storage_descriptor_survives_ancestor_swap_and_rejects_final_symlink() {
        use std::os::unix::fs::symlink;

        let fixture = TempDir::new().expect("fixture");
        let ancestor = fixture.path().join("ancestor");
        let storage = ancestor.join("storage");
        std::fs::create_dir_all(&storage).expect("storage");
        let store = PrivateSubagentMetadataStore::open(&storage).expect("store");
        let parked = fixture.path().join("ancestor-parked");
        std::fs::rename(&ancestor, &parked).expect("park ancestor");
        std::fs::create_dir_all(&storage).expect("replacement storage path");
        store
            .save(record())
            .await
            .expect("save through pinned root");
        assert!(
            parked
                .join("storage/subagents-v1/parent/child.json")
                .is_file()
        );
        assert!(!storage.join("subagents-v1/parent/child.json").exists());

        let final_alias = fixture.path().join("storage-alias");
        symlink(parked.join("storage"), &final_alias).expect("final symlink");
        assert!(
            PrivateSubagentMetadataStore::open(&final_alias).is_err(),
            "final storage symlink must never be traversed"
        );
    }

    #[cfg(not(unix))]
    #[tokio::test]
    async fn portable_fallback_round_trips_without_unix_descriptor_support() {
        let storage = TempDir::new().expect("storage");
        let store = PrivateSubagentMetadataStore::open(storage.path()).expect("store");
        store.save(record()).await.expect("save");
        assert_eq!(
            store
                .load_parent(&rw_types::SessionId("parent".to_owned()))
                .expect("load"),
            vec![record()]
        );
    }
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<(), OrchestrationError> {
    fs::create_dir_all(path)
        .map_err(|error| io_error("create subagent metadata directory", error))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect subagent metadata directory", error))?;
    if !metadata.file_type().is_dir() {
        return Err(session_error("subagent metadata path is not a directory"));
    }
    Ok(())
}

fn validate_component(value: &str) -> Result<(), OrchestrationError> {
    if value.is_empty()
        || value.len() > 160
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(session_error("unsafe subagent metadata identity"));
    }
    Ok(())
}

fn validate_session_id(value: &SessionId) -> Result<(), OrchestrationError> {
    SessionId::validate(&value.0)
        .map_err(|_| session_error("unsafe subagent metadata session identity"))
}

#[allow(clippy::needless_pass_by_value)]
fn io_error(context: &str, error: std::io::Error) -> OrchestrationError {
    session_error(format!("{context}: {error}"))
}

fn session_error(message: impl Into<String>) -> OrchestrationError {
    OrchestrationError::Session(message.into())
}
