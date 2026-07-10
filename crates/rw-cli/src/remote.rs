//! SSH-forwarded remote engine command construction.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemotePermissionMode {
    Strict,
    AutoSafe,
    Yolo,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

#[derive(Clone, Debug)]
pub struct RemoteConfig {
    pub ssh_executable: PathBuf,
    pub host: String,
    pub remote_rw_executable: PathBuf,
    pub remote_socket: PathBuf,
    pub local_socket: PathBuf,
    pub permission_mode: RemotePermissionMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteError {
    StrictPermissionRequired,
    InvalidHost,
    InvalidSocketPath,
    InvalidRemoteExecutable,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::StrictPermissionRequired => "remote sessions require strict permission mode",
            Self::InvalidHost => "remote SSH host is invalid",
            Self::InvalidSocketPath => "remote forwarding requires absolute Unix socket paths",
            Self::InvalidRemoteExecutable => "remote rw executable must be an absolute path",
        })
    }
}

impl std::error::Error for RemoteError {}

impl RemoteConfig {
    pub fn validate(&self) -> Result<(), RemoteError> {
        if self.permission_mode != RemotePermissionMode::Strict {
            return Err(RemoteError::StrictPermissionRequired);
        }
        if self.host.is_empty()
            || self.host.starts_with('-')
            || self
                .host
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(RemoteError::InvalidHost);
        }
        if !is_absolute_socket(&self.remote_socket) || !is_absolute_socket(&self.local_socket) {
            return Err(RemoteError::InvalidSocketPath);
        }
        if !is_safe_absolute_path(&self.remote_rw_executable) {
            return Err(RemoteError::InvalidRemoteExecutable);
        }
        Ok(())
    }

    /// Starts or attaches the remote engine. Detach and strict mode are
    /// unconditional, so a local UI exit never terminates the remote engine.
    pub fn engine_start_command(&self) -> Result<SshCommand, RemoteError> {
        self.validate()?;
        Ok(SshCommand {
            program: self.ssh_executable.clone(),
            args: vec![
                OsString::from("-T"),
                OsString::from("-o"),
                OsString::from("BatchMode=yes"),
                OsString::from("--"),
                OsString::from(&self.host),
                self.remote_rw_executable.as_os_str().to_owned(),
                OsString::from("serve"),
                OsString::from("--detach"),
                OsString::from("--permission-mode"),
                OsString::from("strict"),
                OsString::from("--socket"),
                self.remote_socket.as_os_str().to_owned(),
            ],
        })
    }

    /// Creates a `StreamLocal` Unix-socket tunnel. There is deliberately no TCP
    /// bind address or non-loopback daemon surface.
    pub fn forward_command(&self) -> Result<SshCommand, RemoteError> {
        self.validate()?;
        let forwarding = format!(
            "{}:{}",
            self.local_socket.display(),
            self.remote_socket.display()
        );
        Ok(SshCommand {
            program: self.ssh_executable.clone(),
            args: vec![
                OsString::from("-N"),
                OsString::from("-T"),
                OsString::from("-o"),
                OsString::from("ExitOnForwardFailure=yes"),
                OsString::from("-o"),
                OsString::from("StreamLocalBindUnlink=yes"),
                OsString::from("-L"),
                OsString::from(forwarding),
                OsString::from("--"),
                OsString::from(&self.host),
            ],
        })
    }
}

fn is_absolute_socket(path: &Path) -> bool {
    is_safe_absolute_path(path)
}

fn is_safe_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|value| {
            !value.is_empty()
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-')
                })
        })
}

/// Testable seam proving loopback remote mode uses the same client-side socket
/// consumer as a local engine; only socket establishment differs.
pub trait ForwardedSocketConsumer {
    type Output;
    type Error;

    fn connect(&self, socket: &Path) -> Result<Self::Output, Self::Error>;
}

pub fn connect_forwarded<C: ForwardedSocketConsumer>(
    config: &RemoteConfig,
    consumer: &C,
) -> Result<C::Output, RemoteConnectError<C::Error>> {
    config.validate().map_err(RemoteConnectError::Config)?;
    consumer
        .connect(&config.local_socket)
        .map_err(RemoteConnectError::Consumer)
}

#[derive(Debug)]
pub enum RemoteConnectError<E> {
    Config(RemoteError),
    Consumer(E),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn config() -> RemoteConfig {
        RemoteConfig {
            ssh_executable: PathBuf::from("/usr/bin/ssh"),
            host: "localhost".to_owned(),
            remote_rw_executable: PathBuf::from("/usr/local/bin/rw"),
            remote_socket: PathBuf::from("/tmp/rottweiler/engine.sock"),
            local_socket: PathBuf::from("/tmp/rottweiler-forward.sock"),
            permission_mode: RemotePermissionMode::Strict,
        }
    }

    #[test]
    fn exact_ssh_argv_is_detached_strict_streamlocal_and_secret_free() {
        let config = config();
        let start = config.engine_start_command().expect("start command");
        assert_eq!(
            start.args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "--",
                "localhost",
                "/usr/local/bin/rw",
                "serve",
                "--detach",
                "--permission-mode",
                "strict",
                "--socket",
                "/tmp/rottweiler/engine.sock",
            ]
            .map(OsString::from)
        );
        let forward = config.forward_command().expect("forward command");
        assert_eq!(
            forward.args,
            [
                "-N",
                "-T",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StreamLocalBindUnlink=yes",
                "-L",
                "/tmp/rottweiler-forward.sock:/tmp/rottweiler/engine.sock",
                "--",
                "localhost",
            ]
            .map(OsString::from)
        );
        assert!(!format!("{start:?}{forward:?}").contains("token"));
    }

    #[test]
    fn remote_never_relaxes_permissions_or_accepts_ssh_option_injection() {
        for mode in [RemotePermissionMode::AutoSafe, RemotePermissionMode::Yolo] {
            let mut candidate = config();
            candidate.permission_mode = mode;
            assert_eq!(
                candidate.validate(),
                Err(RemoteError::StrictPermissionRequired)
            );
        }
        let mut candidate = config();
        candidate.host = "-oProxyCommand=bad".to_owned();
        assert_eq!(candidate.validate(), Err(RemoteError::InvalidHost));
        let mut candidate = config();
        candidate.remote_socket = PathBuf::from("/tmp/socket;touch-pwned");
        assert_eq!(candidate.validate(), Err(RemoteError::InvalidSocketPath));
    }

    struct RecordingConsumer(Mutex<Vec<PathBuf>>);

    use std::sync::Mutex;

    impl ForwardedSocketConsumer for RecordingConsumer {
        type Output = &'static str;
        type Error = ();

        fn connect(&self, socket: &Path) -> Result<Self::Output, Self::Error> {
            self.0.lock().expect("paths").push(socket.to_path_buf());
            Ok("same-client-path")
        }
    }

    #[test]
    fn loopback_remote_uses_the_forwarded_local_socket_seam() {
        let consumer = RecordingConsumer(Mutex::new(Vec::new()));
        assert_eq!(
            connect_forwarded(&config(), &consumer).expect("connect"),
            "same-client-path"
        );
        assert_eq!(
            consumer.0.lock().expect("paths").as_slice(),
            [PathBuf::from("/tmp/rottweiler-forward.sock")]
        );
    }
}
