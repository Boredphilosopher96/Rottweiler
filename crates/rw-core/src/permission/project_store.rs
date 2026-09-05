use std::fmt::Write as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
};

use super::{
    PermissionKey, RememberedApproval, contains_approval, lock_mutex, lock_write, replace_approval,
    revoke_approvals,
};

pub(super) struct ProjectApprovalStore {
    pub(super) path: PathBuf,
    pub(super) transaction: Mutex<()>,
    pub(super) cached: RwLock<BTreeSet<RememberedApproval>>,
}

impl ProjectApprovalStore {
    pub(super) fn refresh(&self) -> Result<BTreeSet<RememberedApproval>, std::io::Error> {
        let _transaction = lock_mutex(&self.transaction);
        let _file_lock = CrossProcessApprovalLock::acquire(&self.path)?;
        let approvals = load_project_approvals(&self.path)?;
        lock_write(&self.cached).clone_from(&approvals);
        Ok(approvals)
    }

    pub(super) fn contains(&self, key: &PermissionKey) -> Result<bool, std::io::Error> {
        self.refresh()
            .map(|approvals| contains_approval(&approvals, key))
    }

    pub(super) fn grant(&self, key: PermissionKey) -> Result<(), std::io::Error> {
        self.update(|approvals| {
            if !contains_approval(approvals, &key) {
                let approval = RememberedApproval::new("project", key).ok_or_else(|| {
                    std::io::Error::other("secure random approval id generation failed")
                })?;
                replace_approval(approvals, approval);
            }
            Ok(())
        })
    }

    pub(super) fn revoke(&self, id: Option<&str>) -> Result<usize, std::io::Error> {
        let mut removed = 0;
        self.update(|approvals| {
            removed = revoke_approvals(approvals, id);
            Ok(())
        })?;
        Ok(removed)
    }

    pub(super) fn clear_all(&self) -> Result<usize, std::io::Error> {
        self.revoke(None)
    }

    fn update(
        &self,
        change: impl FnOnce(&mut BTreeSet<RememberedApproval>) -> Result<(), std::io::Error>,
    ) -> Result<(), std::io::Error> {
        let _transaction = lock_mutex(&self.transaction);
        let _file_lock = CrossProcessApprovalLock::acquire(&self.path)?;
        let mut approvals = load_project_approvals(&self.path)?;
        let original = approvals.clone();
        change(&mut approvals)?;
        if approvals != original {
            persist_project_approvals(&self.path, &approvals)?;
        }
        *lock_write(&self.cached) = approvals;
        Ok(())
    }
}

pub(super) fn shared_project_store(path: &Path) -> Arc<ProjectApprovalStore> {
    static STORES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<ProjectApprovalStore>>>> = OnceLock::new();
    let normalized = normalize_approval_path(path);
    let registry = STORES.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut registry = lock_mutex(registry);
    Arc::clone(registry.entry(normalized.clone()).or_insert_with(|| {
        let store = Arc::new(ProjectApprovalStore {
            path: normalized,
            transaction: Mutex::new(()),
            cached: RwLock::new(BTreeSet::new()),
        });
        let _ = store.refresh();
        store
    }))
}

pub(super) fn normalize_approval_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let Some(parent) = absolute.parent() else {
        return absolute;
    };
    fs::canonicalize(parent)
        .map(|parent| parent.join(absolute.file_name().unwrap_or_default()))
        .unwrap_or(absolute)
}

pub(super) struct CrossProcessApprovalLock {
    _file: fs::File,
}

impl CrossProcessApprovalLock {
    fn acquire(path: &Path) -> Result<Self, std::io::Error> {
        #[cfg(not(unix))]
        {
            let _ = path;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "durable project approvals require a supported cross-process file lock",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            fs::create_dir_all(parent)?;
            set_private_directory(parent)?;
            let lock_path = sibling_path(path, "lock")?;
            let mut options = fs::OpenOptions::new();
            options.read(true).write(true).create(true);
            options
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
            let file = options.open(lock_path)?;
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
            Ok(Self { _file: file })
        }
    }
}

pub(super) fn load_project_approvals(
    path: &Path,
) -> Result<BTreeSet<RememberedApproval>, std::io::Error> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "project approval ledger is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "project approval ledger is not private",
            ));
        }
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("project approval ledger is malformed: {error}"),
        )
    })
}

pub(super) fn persist_project_approvals(
    path: &Path,
    approvals: &BTreeSet<RememberedApproval>,
) -> Result<(), std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    set_private_directory(parent)?;
    let encoded = serde_json::to_vec(approvals)
        .map_err(|error| std::io::Error::other(format!("approval encoding failed: {error}")))?;
    let temporary = unique_temporary_path(path)?;
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        if let Err(error) = fs::File::open(parent).and_then(|directory| directory.sync_all()) {
            let _ = fs::remove_file(path);
            let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
            return Err(error);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) fn set_private_directory(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn unique_temporary_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| std::io::Error::other("secure random temp name generation failed"))?;
    let suffix = hex(&random);
    sibling_path(path, &format!("tmp.{suffix}"))
}

pub(super) fn sibling_path(path: &Path, suffix: &str) -> Result<PathBuf, std::io::Error> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "approval ledger has no file name",
        )
    })?;
    let mut sibling = name.to_os_string();
    sibling.push(format!(".{suffix}"));
    Ok(path.with_file_name(sibling))
}

pub(super) fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}
