//! Exact approved bytes retained by the physical process, including failed handoff.
use super::{PluginProcessConfig, PluginProcessError, error, process_error};
use rw_ext::{PluginSandboxMode, PluginSandboxProfile};
use rw_tools::{ApprovedCode, ApprovedExecutable, ExecutableLaunch};
use std::{
    ffi::OsString,
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
    Attested(ApprovedCode),
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
            let files = config
                .attested_files()
                .iter()
                .filter(|identity| identity.canonical_path != config.executable())
                .map(rw_ext::ExecutableIdentity::artifact_identity)
                .collect::<Vec<_>>();
            CodeView::Attested(
                ApprovedCode::capture(config.cwd(), config.argv(), &files)
                    .map_err(|cause| error(&cause.to_string()))?,
            )
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
                code: CodeView::Preparation { cwd, .. },
                ..
            } => cwd,
            Self::Native {
                code: CodeView::Attested(code),
                ..
            } => code.cwd(),
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
                code: CodeView::Preparation { args, .. },
                ..
            } => args,
            Self::Native {
                code: CodeView::Attested(code),
                ..
            } => code.args(),
            #[cfg(target_os = "linux")]
            Self::Preparation { .. } => config.argv(),
            #[cfg(test)]
            Self::Harness { .. } => config.argv(),
        }
    }
    pub(super) fn validate_write_roots(&self, roots: &[PathBuf]) -> Result<(), PluginProcessError> {
        let pinned_roots = self.immutable_roots();
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

    fn immutable_roots(&self) -> Vec<PathBuf> {
        match self {
            Self::Native { executable, code } => {
                let mut roots = Vec::new();
                if let CodeView::Attested(code) = code {
                    roots.push(code.root().to_path_buf());
                }
                if cfg!(target_os = "macos") {
                    roots.push(executable.path().to_path_buf());
                }
                // Preparation source is a separately authorized read view,
                // not an immutable snapshot. Existing output grants remain
                // governed by its owner; executable bytes stay protected.
                roots
            }
            #[cfg(target_os = "linux")]
            Self::Preparation { .. } => Vec::new(),
            #[cfg(test)]
            Self::Harness { .. } => Vec::new(),
        }
    }

    pub(super) fn read_roots(&self) -> Vec<PathBuf> {
        match self {
            Self::Native { executable, code } => {
                let root = match code {
                    CodeView::Attested(code) => code.root(),
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
