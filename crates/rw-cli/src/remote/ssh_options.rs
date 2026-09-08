//! One SSH profile for remote engine, socket forwarding and foreground input.
use std::{ffi::OsString, fs::OpenOptions, os::unix::fs::OpenOptionsExt as _, path::PathBuf};

use super::RemoteError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SshOptions {
    pub executable: PathBuf,
    pub config_file: Option<PathBuf>,
}

impl SshOptions {
    pub fn from_environment() -> Self {
        Self {
            executable: std::env::var_os("ROTTWEILER_SSH_BIN")
                .map_or_else(|| PathBuf::from("/usr/bin/ssh"), PathBuf::from),
            config_file: std::env::var_os("ROTTWEILER_SSH_CONFIG").map(PathBuf::from),
        }
    }

    pub fn validate(&self) -> Result<(), RemoteError> {
        if let Some(path) = &self.config_file {
            if !path.is_absolute() {
                return Err(RemoteError::SshConfig);
            }
            // Nonblocking open rejects FIFOs/devices without waiting for a peer.
            // OpenSSH subsequently reads its normal external configuration file.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(path)
                .map_err(|_| RemoteError::SshConfig)?;
            if !file
                .metadata()
                .map_err(|_| RemoteError::SshConfig)?
                .is_file()
            {
                return Err(RemoteError::SshConfig);
            }
        }
        Ok(())
    }

    pub fn arguments(&self) -> Vec<OsString> {
        self.config_file.as_ref().map_or_else(Vec::new, |path| {
            vec![OsString::from("-F"), path.as_os_str().to_owned()]
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn explicit_profile_rejects_relative_missing_nonregular_and_unreadable_files() {
        let root = tempfile::tempdir().expect("profile directory");
        let mut options = SshOptions {
            executable: "/usr/bin/ssh".into(),
            config_file: None,
        };
        assert!(options.validate().is_ok());
        assert!(options.arguments().is_empty());
        for path in [
            PathBuf::from("relative"),
            root.path().join("missing"),
            root.path().to_owned(),
            PathBuf::from("/dev/null"),
        ] {
            options.config_file = Some(path);
            assert_eq!(options.validate(), Err(RemoteError::SshConfig));
        }
        let config = root.path().join("custom profile");
        std::fs::write(&config, "Host selected\n HostName localhost\n").expect("profile");
        options.config_file = Some(config.clone());
        assert!(options.validate().is_ok());
        assert_eq!(
            options.arguments(),
            [OsString::from("-F"), config.as_os_str().to_owned()]
        );
        if rustix::process::geteuid().as_raw() != 0 {
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0))
                .expect("deny read");
            assert_eq!(options.validate(), Err(RemoteError::SshConfig));
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
                .expect("restore access");
        }
    }
}
