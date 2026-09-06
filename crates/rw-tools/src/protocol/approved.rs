//! Exact content authority supplied by the host's MCP approval owner.
use super::{
    ProtocolChildRequest, ProtocolSandboxPolicy, trusted_executable, validate_request_bounds,
};
use rw_sandbox::{ApprovedCode, ApprovedExecutable, ExecutableArtifactIdentity, ExecutableLaunch};
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Captured approved command; callers must obtain approval before constructing it.
/// The physical process retains this owner, not just the connector that launched it.
pub struct ApprovedProtocolCommand {
    executable: ApprovedExecutable,
    code: ApprovedCode,
    binding: Binding,
}
struct Binding {
    executable: PathBuf,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    sandbox: ProtocolSandboxPolicy,
    environment: [u8; 32],
}
impl ApprovedProtocolCommand {
    /// Copies the exact approved executable and interpreter inputs before effects.
    /// Run this bounded filesystem work in an admitted host worker.
    ///
    /// # Errors
    /// Rejects changed approval bytes, untrusted executable location, and bad bounds.
    pub fn capture(
        request: &ProtocolChildRequest,
        executable: &ExecutableArtifactIdentity,
        files: &[ExecutableArtifactIdentity],
        workspace_roots: &[PathBuf],
        cwd: &Path,
    ) -> io::Result<Self> {
        validate_request_bounds(request)?;
        if trusted_executable(&request.executable, workspace_roots)? != executable.executable {
            return Err(io::Error::other(
                "MCP executable differs from its approved identity",
            ));
        }
        let executable = ApprovedExecutable::from_artifact(executable).map_err(io::Error::other)?;
        let args = request.args.iter().map(OsString::from).collect::<Vec<_>>();
        let code = ApprovedCode::capture(cwd, &args, files).map_err(io::Error::other)?;
        Ok(Self {
            executable,
            code,
            binding: Binding {
                executable: request.executable.clone(),
                args: request.args.clone(),
                cwd: request.working_directory.clone(),
                sandbox: request.sandbox.clone(),
                environment: environment_identity(&request.environment),
            },
        })
    }
    pub(super) fn prepare(
        self: &Arc<Self>,
        request: &ProtocolChildRequest,
    ) -> io::Result<PreparedProtocol> {
        if self.binding.executable != request.executable
            || self.binding.args != request.args
            || self.binding.cwd != request.working_directory
            || self.binding.sandbox != request.sandbox
            || self.binding.environment != environment_identity(&request.environment)
        {
            return Err(io::Error::other(
                "protocol request differs from its captured approval",
            ));
        }
        let launch = self.executable.launch().map_err(io::Error::other)?;
        let program = launch.path().to_path_buf();
        let mut read_roots = vec![self.code.root().to_path_buf()];
        if cfg!(target_os = "macos") {
            read_roots.push(program.clone());
        }
        Ok(PreparedProtocol {
            program,
            args: self.code.args().to_vec(),
            read_roots,
            authority: ProcessExecutable::Approved {
                _command: Arc::clone(self),
                _launch: launch,
            },
        })
    }
}

pub(super) struct PreparedProtocol {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub read_roots: Vec<PathBuf>,
    pub authority: ProcessExecutable,
}

pub(crate) enum ProcessExecutable {
    TrustedBinary,
    Approved {
        _command: Arc<ApprovedProtocolCommand>,
        _launch: ExecutableLaunch,
    },
}

fn environment_identity(values: &[(String, String)]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    for (name, value) in values {
        for text in [name, value] {
            hash.update(&text.len().to_le_bytes());
            hash.update(text.as_bytes());
        }
    }
    *hash.finalize().as_bytes()
}

#[cfg(all(test, unix))]
mod tests;
