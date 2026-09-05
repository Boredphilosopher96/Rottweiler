use std::{
    fs, io,
    path::{Path, PathBuf},
};

use miette::{IntoDiagnostic, Result, miette};

use crate::{remote, server};

/// Normalizes rustix's platform-native device identifier without assuming the
/// signed/unsigned width selected by a particular Unix libc ABI.
#[cfg(unix)]
pub(crate) fn rustix_device_id<T: TryInto<u64>>(device: T) -> Option<u64> {
    device.try_into().ok()
}

/// Widens rustix's platform-native mode representation for stable bit tests.
#[cfg(unix)]
#[cfg(test)]
pub(crate) fn rustix_mode_bits<T: Into<u32>>(mode: T) -> u32 {
    mode.into()
}

pub(super) struct RuntimeDirectoryGuard {
    pub(super) path: PathBuf,
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) owner: u32,
    pub(super) armed: bool,
}

pub(super) fn create_guarded_server_runtime(
    paths: server::ServerRuntimePaths,
    session_id: Option<&str>,
) -> Result<(
    RuntimeDirectoryGuard,
    server::ServerRuntime,
    std::os::unix::net::UnixListener,
)> {
    // Selected remote paths may not exist on first attach. Let the server's
    // owner/private path validation create the leaf before capturing its exact
    // identity for lifecycle cleanup.
    let (runtime, listener) = server::ServerRuntime::create_for_session(paths, session_id)?;
    let runtime_directory = RuntimeDirectoryGuard::capture(&runtime.paths.directory)?;
    Ok((runtime_directory, runtime, listener))
}

impl RuntimeDirectoryGuard {
    pub(super) fn capture(path: &Path) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let metadata = fs::symlink_metadata(path).into_diagnostic()?;
        let owner = rustix::process::geteuid().as_raw();
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != owner
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return Err(miette!(
                "runtime directory is not one owner-private directory"
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            owner,
            armed: true,
        })
    }

    pub(super) fn preserve(&mut self) {
        self.armed = false;
    }

    pub(super) fn validate_identity(&self) -> io::Result<()> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let metadata = fs::symlink_metadata(&self.path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != self.owner
            || metadata.permissions().mode() & 0o777 != 0o700
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime directory identity changed before cleanup",
            ));
        }
        Ok(())
    }

    pub(super) fn cleanup(&mut self) -> io::Result<()> {
        use std::os::unix::fs::FileTypeExt as _;

        if !self.armed {
            return Ok(());
        }
        if matches!(
            fs::symlink_metadata(&self.path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ) {
            // A supervised serve child may have already removed the exact
            // shared runtime leaf during its own orderly shutdown.
            self.armed = false;
            return Ok(());
        }
        self.validate_identity()?;
        let entries = fs::read_dir(&self.path)?.collect::<io::Result<Vec<_>>>()?;
        for entry in entries {
            let name = entry.file_name();
            let metadata = fs::symlink_metadata(entry.path())?;
            let expected_type = if name == "engine.sock" {
                metadata.file_type().is_socket()
            } else if matches!(
                name.to_str(),
                Some("auth.token" | "runtime.json" | "last-seen")
            ) {
                metadata.is_file() && !metadata.file_type().is_symlink()
            } else {
                false
            };
            if !expected_type {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runtime directory contains an unexpected artifact",
                ));
            }
            self.validate_identity()?;
            fs::remove_file(entry.path())?;
        }
        self.validate_identity()?;
        fs::remove_dir(&self.path)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for RuntimeDirectoryGuard {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            tracing::warn!(
                path = %self.path.display(),
                reason = %error,
                "left runtime directory in place because safe cleanup could not be proven"
            );
        }
    }
}

pub(super) const MAX_UNIX_SOCKET_PATH_BYTES: usize = 103;

pub(super) fn runtime_root(storage_root: &Path) -> PathBuf {
    let configured = storage_root.join("run");
    let longest_child = configured
        .join("engine-0000000000000000")
        .join("engine.sock");
    if longest_child.as_os_str().as_encoded_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES {
        return configured;
    }

    let digest = blake3::hash(storage_root.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    let name = format!(
        "rottweiler-{}-{}",
        rustix::process::geteuid().as_raw(),
        &digest[..16]
    );
    let preferred = std::env::temp_dir().join(&name);
    let preferred_socket = preferred
        .join("engine-0000000000000000")
        .join("engine.sock");
    if preferred_socket.as_os_str().as_encoded_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES {
        preferred
    } else {
        PathBuf::from("/tmp").join(name)
    }
}

pub(super) fn ensure_private_runtime_root(root: &Path) -> Result<()> {
    fs::create_dir_all(root).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = fs::symlink_metadata(root).into_diagnostic()?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(miette!(
                "engine runtime root must be a real directory owned by the current user: {}",
                root.display()
            ));
        }
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).into_diagnostic()?;
    }
    Ok(())
}

pub(super) fn allocate_runtime_paths(storage_root: &Path) -> Result<server::ServerRuntimePaths> {
    let root = runtime_root(storage_root);
    ensure_private_runtime_root(&root)?;
    for _ in 0..32 {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).into_diagnostic()?;
        let suffix = random
            .iter()
            .fold(String::with_capacity(16), |mut value, byte| {
                use std::fmt::Write as _;
                let _ = write!(&mut value, "{byte:02x}");
                value
            });
        let directory = root.join(format!("engine-{suffix}"));
        match fs::create_dir(&directory) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                        .into_diagnostic()?;
                }
                return Ok(server::ServerRuntimePaths {
                    socket: directory.join("engine.sock"),
                    token: directory.join("auth.token"),
                    descriptor: directory.join("runtime.json"),
                    directory,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).into_diagnostic(),
        }
    }
    Err(miette!("could not allocate an engine runtime directory"))
}

pub(super) fn resolve_server_paths(
    socket: Option<PathBuf>,
    token_file: Option<PathBuf>,
    storage_root: &Path,
) -> Result<server::ServerRuntimePaths> {
    let socket = socket.or_else(|| std::env::var_os("ROTTWEILER_ENGINE_SOCKET").map(PathBuf::from));
    if let Some(socket) = socket {
        let directory = socket
            .parent()
            .ok_or_else(|| miette!("engine socket has no parent directory"))?
            .to_path_buf();
        let token = token_file
            .or_else(|| std::env::var_os("ROTTWEILER_ENGINE_TOKEN_FILE").map(PathBuf::from))
            .unwrap_or_else(|| directory.join("auth.token"));
        return Ok(server::ServerRuntimePaths {
            socket,
            token,
            descriptor: directory.join("runtime.json"),
            directory,
        });
    }
    if token_file.is_some() {
        return Err(miette!("--token-file requires --socket"));
    }
    allocate_runtime_paths(storage_root)
}

pub(super) fn locate_tui_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().into_diagnostic()?;
    let development =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/tui/dist/rottweiler-tui");
    resolve_tui_executable(
        &current,
        std::env::var_os("ROTTWEILER_TUI_BIN").map(PathBuf::from),
        &development,
    )
}

pub(super) fn resolve_tui_executable(
    current_executable: &Path,
    override_path: Option<PathBuf>,
    development_path: &Path,
) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return require_executable(path);
    }
    // Package managers expose a public launcher through a symlink while
    // keeping the complete runtime in a private directory.
    // Resolve the executable that is actually running before looking for its
    // TUI sibling; never derive a helper path from an untrusted PATH entry.
    let installed = fs::canonicalize(current_executable).into_diagnostic()?;
    if let Some(sibling) = installed
        .parent()
        .map(|parent| parent.join("rottweiler-tui"))
        && sibling.is_file()
    {
        return require_executable(sibling);
    }
    require_executable(development_path.to_owned())
}

pub(super) fn require_executable(path: PathBuf) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(&path).into_diagnostic()?;
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = true;
    if metadata.file_type().is_symlink() || !metadata.is_file() || !executable {
        return Err(miette!(
            "compiled OpenTUI executable is not a regular executable at {}; run `bun run build` in packages/tui or set ROTTWEILER_TUI_BIN",
            path.display()
        ));
    }
    Ok(path)
}

pub(super) fn session_metadata_path(storage_root: &Path, session_id: &str) -> PathBuf {
    storage_root
        .join("sessions")
        .join(session_id)
        .join("metadata.json")
}

pub(super) async fn runtime_is_live(paths: &server::ServerRuntimePaths) -> bool {
    if !runtime_artifacts_ready(paths) {
        return false;
    }
    let Ok(Some(token)) = read_private_bootstrap_token(&paths.token) else {
        return false;
    };
    remote::probe_authenticated_health(&paths.socket, &token, std::time::Duration::from_millis(500))
        .await
        .unwrap_or(false)
}

pub(super) fn runtime_artifacts_ready(paths: &server::ServerRuntimePaths) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        let socket_ready = fs::symlink_metadata(&paths.socket).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink() && metadata.file_type().is_socket()
        });
        let token_ready = fs::symlink_metadata(&paths.token).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink() && metadata.is_file() && metadata.len() == 64
        });
        socket_ready && token_ready
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub(super) fn valid_bootstrap_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn read_private_bootstrap_token(path: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(miette!(
            "remote bootstrap-token file is not private and regular"
        ));
    }
    let token = fs::read_to_string(path).into_diagnostic()?;
    let token = token.trim();
    if valid_bootstrap_token(token) {
        Ok(Some(token.to_owned()))
    } else {
        Ok(None)
    }
}

pub(super) fn write_private_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(miette!(
                    "refusing to replace an unsafe remote bootstrap-token file"
                ));
            }
            if fs::read(path).into_diagnostic()? == bytes {
                return Ok(());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).into_diagnostic(),
    }

    let parent = path
        .parent()
        .ok_or_else(|| miette!("remote bootstrap-token file has no parent"))?;
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).into_diagnostic()?;
    let suffix = random.iter().fold(String::new(), |mut output, byte| {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
        output
    });
    let temporary = parent.join(format!(".auth.token.{suffix}.tmp"));

    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()?;
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).into_diagnostic();
    }
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .into_diagnostic()
}

pub(super) fn remove_stale_forward_socket(path: &Path) -> std::result::Result<(), String> {
    use std::os::unix::fs::FileTypeExt as _;

    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.file_type().is_socket() => {
            fs::remove_file(path)
                .map_err(|error| format!("could not remove stale forwarded socket: {error}"))
        }
        Ok(_) => Err("refusing to replace an unexpected forwarded-socket artifact".to_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not inspect forwarded socket: {error}")),
    }
}
