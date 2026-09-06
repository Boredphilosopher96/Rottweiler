//! Exact approved bytes retained by the physical process, including failed handoff.
use super::{PluginProcessConfig, PluginProcessError, error, process_error};
use rw_ext::{PluginSandboxMode, PluginSandboxProfile};
use rw_tools::{ApprovedExecutable, ExecutableLaunch};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

pub(super) enum LaunchBytes {
    Native {
        executable: ExecutableLaunch,
        code: CodeView,
    },
    // The Linux preparation helper establishes its own sealed compiler and
    // projected filesystem before exec. Its required layout is the authority.
    #[cfg(target_os = "linux")]
    Preparation {
        _layout: rw_tools::PreparationFilesystem,
    },
    #[cfg(test)]
    Harness { _helper: rw_tools::SandboxHelper },
}

pub(super) enum CodeView {
    Attested {
        directory: tempfile::TempDir,
        cwd: PathBuf,
        args: Vec<OsString>,
    },
    Preparation {
        root: PathBuf,
        cwd: PathBuf,
        args: Vec<OsString>,
    },
}

impl LaunchBytes {
    pub(super) fn capture(
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> Result<Self, PluginProcessError> {
        #[cfg(target_os = "linux")]
        if let PluginSandboxMode::Preparation { filesystem } = &profile.mode {
            return Ok(Self::Preparation {
                _layout: filesystem.as_ref().clone(),
            });
        }
        let executable =
            ApprovedExecutable::from_artifact(&config.executable_identity().artifact_identity())
                .and_then(|approved| approved.launch())
                .map_err(|cause| error(&cause.to_string()))?;
        let code = if matches!(profile.mode, PluginSandboxMode::Preparation { .. }) {
            CodeView::Preparation {
                root: config
                    .code_root()
                    .ok_or_else(|| error("preparation code root missing"))?
                    .canonical_path
                    .clone(),
                cwd: config.cwd().to_path_buf(),
                args: config.argv().to_vec(),
            }
        } else {
            CodeView::capture(config)?
        };
        Ok(Self::Native { executable, code })
    }

    pub(super) fn program<'a>(&'a self, config: &'a PluginProcessConfig) -> &'a Path {
        #[cfg(not(any(target_os = "linux", test)))]
        let _ = config;
        match self {
            Self::Native { executable, .. } => executable.path(),
            #[cfg(target_os = "linux")]
            Self::Preparation { .. } => config.executable(),
            #[cfg(test)]
            Self::Harness { .. } => config.executable(),
        }
    }
    pub(super) fn cwd<'a>(&'a self, config: &'a PluginProcessConfig) -> &'a Path {
        #[cfg(not(any(target_os = "linux", test)))]
        let _ = config;
        match self {
            Self::Native {
                code: CodeView::Attested { cwd, .. } | CodeView::Preparation { cwd, .. },
                ..
            } => cwd,
            #[cfg(target_os = "linux")]
            Self::Preparation { .. } => config.cwd(),
            #[cfg(test)]
            Self::Harness { .. } => config.cwd(),
        }
    }
    pub(super) fn args<'a>(&'a self, config: &'a PluginProcessConfig) -> &'a [OsString] {
        #[cfg(not(any(target_os = "linux", test)))]
        let _ = config;
        match self {
            Self::Native {
                code: CodeView::Attested { args, .. } | CodeView::Preparation { args, .. },
                ..
            } => args,
            #[cfg(target_os = "linux")]
            Self::Preparation { .. } => config.argv(),
            #[cfg(test)]
            Self::Harness { .. } => config.argv(),
        }
    }
    pub(super) fn validate_write_roots(&self, roots: &[PathBuf]) -> Result<(), PluginProcessError> {
        let pinned_roots = self.read_roots();
        for root in roots {
            let root = root.canonicalize().map_err(|cause| process_error(&cause))?;
            for pinned in &pinned_roots {
                let pinned = pinned
                    .canonicalize()
                    .map_err(|cause| process_error(&cause))?;
                if pinned.starts_with(&root) || root.starts_with(&pinned) {
                    return Err(error(
                        "plugin writable scratch overlaps its approved immutable bytes",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn read_roots(&self) -> Vec<PathBuf> {
        match self {
            Self::Native { executable, code } => {
                let root = match code {
                    CodeView::Attested { directory, .. } => directory.path(),
                    CodeView::Preparation { root, .. } => root.as_path(),
                };
                let mut roots = vec![root.to_path_buf()];
                // Sealed memfds are anonymous inodes, not Landlock path roots.
                // Darwin's private snapshot instead needs an exact file grant.
                if cfg!(target_os = "macos") {
                    roots.push(executable.path().to_path_buf());
                }
                roots
            }
            #[cfg(target_os = "linux")]
            Self::Preparation { .. } => Vec::new(),
            #[cfg(test)]
            Self::Harness { .. } => Vec::new(),
        }
    }
}

impl CodeView {
    fn capture(config: &PluginProcessConfig) -> Result<Self, PluginProcessError> {
        let directory = tempfile::Builder::new()
            .prefix("rw-approved-plugin-")
            .tempdir()
            .map_err(|cause| process_error(&cause))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .map_err(|cause| process_error(&cause))?;
        }
        let root = directory
            .path()
            .canonicalize()
            .map_err(|cause| process_error(&cause))?;
        let project = |path: &Path| -> Result<PathBuf, PluginProcessError> {
            Ok(root.join(
                path.strip_prefix("/")
                    .map_err(|_| error("attested code path is not absolute"))?,
            ))
        };
        for identity in config.attested_files() {
            if identity.canonical_path == config.executable() {
                continue;
            }
            let destination = project(&identity.canonical_path)?;
            let parent = destination
                .parent()
                .ok_or_else(|| error("attested code parent missing"))?;
            fs::create_dir_all(parent).map_err(|cause| process_error(&cause))?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|cause| process_error(&cause))?;
            identity
                .artifact_identity()
                .copy_verified(&mut file)
                .map_err(|cause| error(&cause.to_string()))?;
            let mut permissions = file
                .metadata()
                .map_err(|cause| process_error(&cause))?
                .permissions();
            permissions.set_readonly(true);
            file.set_permissions(permissions)
                .map_err(|cause| process_error(&cause))?;
        }
        let cwd = project(config.cwd())?;
        fs::create_dir_all(&cwd).map_err(|cause| process_error(&cause))?;
        let args = config
            .argv()
            .iter()
            .map(|argument| {
                let canonical = Path::new(argument);
                if config.attested_files().iter().any(|identity| {
                    identity.canonical_path == canonical && canonical != config.executable()
                }) {
                    project(canonical).map(PathBuf::into_os_string)
                } else {
                    Ok(argument.clone())
                }
            })
            .collect::<Result<Vec<_>, PluginProcessError>>()?;
        Ok(Self::Attested {
            directory,
            cwd,
            args,
        })
    }
}
