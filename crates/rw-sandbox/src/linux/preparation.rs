//! Creates the source compiler view before dropping mount and network authority.

use super::{command_without_helper_pin, install_landlock, install_network_floor, sandbox_backend};
use crate::{NetworkPolicy, PreparationFilesystem, SandboxError, SandboxPolicy, preparation::Root};
use rustix::mount::{MountFlags, mount, mount_bind, mount_remount};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd as _,
        unix::{fs::OpenOptionsExt as _, process::CommandExt as _},
    },
    path::{Path, PathBuf},
};

const MAX_VIEW_NODES: usize = 512;
const MAX_VIEW_ENTRIES: usize = 8192;

pub(super) fn run(
    policy: &SandboxPolicy,
    layout: &PreparationFilesystem,
    program: &OsStr,
    args: &[OsString],
    helper_pin: Option<u32>,
) -> Result<std::convert::Infallible, SandboxError> {
    layout.validate()?;
    if policy.network != NetworkPolicy::Deny || policy.read_roots.is_some() {
        return Err(SandboxError::MalformedHelper);
    }
    let expected = std::iter::once(&layout.work.path)
        .chain(layout.output.iter().map(|root| &root.path))
        .collect::<Vec<_>>();
    if policy
        .write_roots
        .iter()
        .any(|root| !expected.contains(&root))
    {
        return Err(SandboxError::MalformedHelper);
    }
    let args = args
        .iter()
        .map(|arg| layout.project_argument(arg))
        .collect::<Result<Vec<_>, _>>()?;
    let mut view = View::new(&layout.mount)?;
    view.bind_declared(&layout.code, "plugin", &layout.credentials, true)?;
    view.bind_declared(&layout.work, "scratch", &[], false)?;
    if let Some(output) = &layout.output {
        view.bind_declared(output, "output", &[], false)?;
    }
    let runtime_exclusions = layout
        .homes
        .iter()
        .chain(&layout.credentials)
        .cloned()
        .collect::<Vec<_>>();
    for root in [
        "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32",
    ] {
        let path = Path::new(root);
        if path.exists() {
            view.bind_tree(
                path,
                Path::new(root.trim_start_matches('/')),
                &runtime_exclusions,
                true,
            )?;
        }
    }
    view.directory(Path::new("dev"))?;
    for (name, readonly) in [("null", false), ("urandom", true), ("zero", true)] {
        view.bind_tree(
            &Path::new("/dev").join(name),
            &Path::new("dev").join(name),
            &[],
            readonly,
        )?;
    }
    view.directory(Path::new("proc"))?;
    mount(
        "proc",
        view.target(Path::new("proc")),
        "proc",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC | MountFlags::RDONLY,
        None,
    )
    .map_err(sandbox_backend)?;
    view.directory(Path::new("host"))?;
    view.bind_tree(
        Path::new(program),
        Path::new("host/source-host"),
        &layout.credentials,
        true,
    )?;
    nix::unistd::fchdir(&view.root).map_err(sandbox_backend)?;
    nix::unistd::chroot(".").map_err(sandbox_backend)?;
    std::env::set_current_dir("/").map_err(sandbox_backend)?;
    drop(view);
    let mut writable = policy
        .write_roots
        .iter()
        .map(|root| layout.project_argument(root.as_os_str()).map(PathBuf::from))
        .collect::<Result<Vec<_>, _>>()?;
    writable.push(PathBuf::from("/dev/null"));
    let projected =
        SandboxPolicy::new(writable, NetworkPolicy::Deny)?.with_read_roots([Path::new("/")])?;
    install_landlock(&projected, &OsString::from("/host/source-host"))?;
    install_network_floor(false)?;
    lock_mount_authority()?;
    let mut command =
        command_without_helper_pin(&OsString::from("/host/source-host"), &args, helper_pin)?;
    command
        .current_dir("/plugin")
        .env("HOME", "/scratch")
        .env("TMPDIR", "/scratch");
    Err(SandboxError::Exec(command.exec()))
}

struct View {
    root: File,
    nodes: usize,
    entries: usize,
}
impl View {
    fn new(root: &Root) -> Result<Self, SandboxError> {
        let pinned = open_root(root)?;
        if fs::read_dir(fd_path(&pinned))
            .map_err(sandbox_backend)?
            .next()
            .is_some()
        {
            return Err(SandboxError::MalformedHelper);
        }
        Ok(Self {
            root: pinned,
            nodes: 0,
            entries: 0,
        })
    }
    fn target(&self, relative: &Path) -> PathBuf {
        fd_path(&self.root).join(relative)
    }
    fn node(&mut self) -> Result<(), SandboxError> {
        self.nodes += 1;
        if self.nodes > MAX_VIEW_NODES {
            return Err(SandboxError::Unavailable(
                "source preparation view exceeds its mount-entry limit".to_owned(),
            ));
        }
        Ok(())
    }
    fn directory(&mut self, relative: &Path) -> Result<(), SandboxError> {
        self.node()?;
        fs::create_dir(self.target(relative)).map_err(sandbox_backend)
    }
    fn bind_declared(
        &mut self,
        root: &Root,
        relative: &str,
        excluded: &[PathBuf],
        readonly: bool,
    ) -> Result<(), SandboxError> {
        let pinned = open_root(root)?;
        self.bind_pinned(&pinned, &root.path, Path::new(relative), excluded, readonly)
    }
    fn bind_tree(
        &mut self,
        source: &Path,
        relative: &Path,
        excluded: &[PathBuf],
        readonly: bool,
    ) -> Result<(), SandboxError> {
        let source = fs::canonicalize(source).map_err(sandbox_backend)?;
        let pinned = open_path(&source)?;
        self.bind_pinned(&pinned, &source, relative, excluded, readonly)
    }
    fn bind_pinned(
        &mut self,
        source: &File,
        logical: &Path,
        relative: &Path,
        excluded: &[PathBuf],
        readonly: bool,
    ) -> Result<(), SandboxError> {
        if excluded.iter().any(|path| logical.starts_with(path)) {
            return Ok(());
        }
        let metadata = source.metadata().map_err(sandbox_backend)?;
        if metadata.is_dir() && excluded.iter().any(|path| path.starts_with(logical)) {
            self.directory(relative)?;
            for entry in fs::read_dir(fd_path(source)).map_err(sandbox_backend)? {
                let entry = entry.map_err(sandbox_backend)?;
                self.entries += 1;
                if self.entries > MAX_VIEW_ENTRIES {
                    return Err(SandboxError::Unavailable(
                        "source preparation view exceeds its directory-entry limit".to_owned(),
                    ));
                }
                let name = entry.file_name();
                if excluded
                    .iter()
                    .any(|path| logical.join(&name).starts_with(path))
                {
                    continue;
                }
                let child = open_path(&entry.path())?;
                if child
                    .metadata()
                    .map_err(sandbox_backend)?
                    .file_type()
                    .is_symlink()
                {
                    self.node()?;
                    std::os::unix::fs::symlink(
                        fs::read_link(entry.path()).map_err(sandbox_backend)?,
                        self.target(&relative.join(name)),
                    )
                    .map_err(sandbox_backend)?;
                } else {
                    self.bind_pinned(
                        &child,
                        &logical.join(&name),
                        &relative.join(name),
                        excluded,
                        readonly,
                    )?;
                }
            }
            return Ok(());
        }
        if metadata.is_dir() {
            self.directory(relative)?;
        } else {
            self.node()?;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.target(relative))
                .map_err(sandbox_backend)?;
        }
        let target = self.target(relative);
        mount_bind(fd_path(source), &target).map_err(sandbox_backend)?;
        if readonly {
            mount_remount(
                target,
                MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID,
                "",
            )
            .map_err(sandbox_backend)?;
        }
        Ok(())
    }
}
fn open_root(root: &Root) -> Result<File, SandboxError> {
    let file = open_path(&root.path)?;
    if !root.matches(&file.metadata().map_err(sandbox_backend)?) {
        return Err(SandboxError::RootTypeChanged);
    }
    Ok(file)
}
fn open_path(path: &Path) -> Result<File, SandboxError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(sandbox_backend)
}
fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
fn lock_mount_authority() -> Result<(), SandboxError> {
    use rustix::thread::{CapabilitiesSecureBits as Bits, CapabilitySet, CapabilitySets};
    rustix::thread::clear_ambient_capability_set().map_err(sandbox_backend)?;
    rustix::thread::set_capabilities_secure_bits(
        Bits::NO_ROOT
            | Bits::NO_ROOT_LOCKED
            | Bits::NO_CAP_AMBIENT_RAISE
            | Bits::NO_CAP_AMBIENT_RAISE_LOCKED
            | Bits::KEEP_CAPS_LOCKED,
    )
    .map_err(sandbox_backend)?;
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )
    .map_err(sandbox_backend)?;
    let denied = [
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_chroot,
        libc::SYS_pivot_root,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_open_by_handle_at,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_mount_setattr,
    ]
    .into_iter()
    .map(|syscall| (syscall, Vec::new()))
    .collect::<BTreeMap<_, _>>();
    let filter: BpfProgram = SeccompFilter::new(
        denied,
        SeccompAction::Allow,
        SeccompAction::Errno(u32::try_from(libc::EPERM).map_err(sandbox_backend)?),
        std::env::consts::ARCH.try_into().map_err(sandbox_backend)?,
    )
    .map_err(sandbox_backend)?
    .try_into()
    .map_err(sandbox_backend)?;
    seccompiler::apply_filter(&filter).map_err(sandbox_backend)
}
