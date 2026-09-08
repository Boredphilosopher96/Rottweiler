//! Single-process plugin supervision with unreaped child and parent authority.
mod control;
use crate::{LaunchPlan, SandboxError};
pub use control::PluginLifeline;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::ExitStatusExt as _,
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) const ENTRY: &str = "--rw-supervise-plugin";
const HELLO: &[u8; 8] = b"RWPLIFE1";
const WAIT: Duration = Duration::from_millis(5);
const HANDSHAKE: Duration = Duration::from_secs(5);

/// Private, bounded rendezvous outside the sandbox's writable directories.
/// The plugin cannot inherit its descriptors or keep its parent's lifeline open.
pub struct PluginRendezvous {
    listener: UnixListener,
    directory: tempfile::TempDir,
}
impl PluginRendezvous {
    /// Create the connection owner before any child is spawned.
    /// # Errors
    /// Fails if private directory or socket creation fails.
    pub fn bind() -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("rw-plugin-owner-")
            // Unix socket addresses have a small fixed path capacity. The
            // private namespace uses /tmp independently of ambient TMPDIR.
            .tempdir_in("/tmp")?;
        let listener = UnixListener::bind(directory.path().join("owner.sock"))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            directory,
        })
    }
    /// Wrap the complete sandbox plan in the approved helper supervisor.
    /// # Errors
    /// Rejects an unavailable immutable helper launch descriptor.
    pub fn wrap(&self, plan: &mut LaunchPlan) -> Result<(), SandboxError> {
        if !plan.single_process {
            return Err(SandboxError::Unavailable(
                "plugin supervision requires a single-process sandbox".into(),
            ));
        }
        #[cfg(target_os = "linux")]
        let helper = {
            use std::os::fd::AsRawFd as _;
            let pin = plan
                .helper_pin
                .as_ref()
                .ok_or_else(|| SandboxError::Unavailable("missing supervisor helper pin".into()))?;
            std::path::PathBuf::from(format!("/proc/self/fd/{}", pin.as_raw_fd()))
        };
        #[cfg(not(target_os = "linux"))]
        let helper = plan.helper.launch_path().to_path_buf();
        let mut args = vec![
            OsString::from(ENTRY),
            self.directory.path().join("owner.sock").into_os_string(),
            plan.program.as_os_str().to_owned(),
        ];
        args.append(&mut plan.args);
        plan.program = helper;
        plan.args = args;
        Ok(())
    }
    /// Verify the spawned supervisor and remove its private socket pathname.
    /// The returned control must grant launch explicitly; dropping it before
    /// that grant cannot start the effect process.
    /// # Errors
    /// Rejects a missing, malformed, or late supervisor handshake.
    pub fn accept(self, pid: u32) -> io::Result<PluginLifeline> {
        let deadline = Instant::now() + HANDSHAKE;
        let mut stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(WAIT);
                }
                Err(error) => return Err(error),
            }
        };
        #[cfg(target_os = "macos")]
        let actual_pid =
            nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::LocalPeerPid)
                .map_err(io::Error::other)?;
        #[cfg(target_os = "linux")]
        let actual_pid =
            nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
                .map_err(io::Error::other)?
                .pid();
        if u32::try_from(actual_pid).ok() != Some(pid) {
            return Err(io::Error::other("unexpected plugin supervisor peer"));
        }
        stream.set_read_timeout(Some(
            deadline.saturating_duration_since(Instant::now()).max(WAIT),
        ))?;
        stream.set_write_timeout(Some(WAIT))?;
        let mut hello = [0; 12];
        stream.read_exact(&mut hello)?;
        if &hello[..8] != HELLO || hello[8..] != pid.to_be_bytes() {
            return Err(io::Error::other("invalid plugin supervisor identity"));
        }
        std::fs::remove_file(self.directory.path().join("owner.sock"))?;
        PluginLifeline::new(stream)
    }
}

pub(crate) fn run(args: &[OsString]) -> io::Result<std::convert::Infallible> {
    let [_, _, socket, program, arguments @ ..] = args else {
        return Err(io::Error::other("invalid plugin supervisor invocation"));
    };
    if rustix::process::getpgrp() != rustix::process::getpid() {
        return Err(io::Error::other(
            "plugin supervisor must own its process group",
        ));
    }
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(HELLO)?;
    stream.write_all(&std::process::id().to_be_bytes())?;
    let mut grant = [0];
    stream.read_exact(&mut grant)?;
    if grant != [1] {
        return Err(io::Error::other("plugin launch was not granted"));
    }
    stream.set_nonblocking(true)?;
    #[cfg(target_os = "linux")]
    rustix::process::set_child_subreaper(Some(rustix::process::getpid()))?;
    let child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut owner = ChildGroup::new(child);
    loop {
        match stream.read(&mut grant) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            _ => break,
        }
        match rustix::process::waitid(
            WaitId::Pid(owner.pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(None) => std::thread::sleep(WAIT),
            // Any failed observation goes through physical retirement too.
            Ok(Some(_)) | Err(_) => break,
        }
    }
    let status = owner.retire();
    stream.write_all(control::DONE)?;
    std::process::exit(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
    )
}

/// The helper owns its actual child until kill and wait complete. The parent
/// separately owns the helper as the process-group anchor through helper exit.
struct ChildGroup {
    child: Child,
    pid: Pid,
    retired: bool,
}
impl ChildGroup {
    fn new(child: Child) -> Self {
        let pid = Pid::from_child(&child);
        Self {
            child,
            pid,
            retired: false,
        }
    }
    fn retire(&mut self) -> std::process::ExitStatus {
        loop {
            match rustix::process::kill_process(self.pid, Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => break,
                Err(_) => std::thread::sleep(WAIT),
            }
        }
        let status = loop {
            match self.child.wait() {
                Ok(status) => break status,
                Err(_) => std::thread::sleep(WAIT),
            }
        };
        // Linux's namespace launcher can exit before its namespace init.
        // This single-threaded supervisor is their subreaper and owns every
        // adopted wait result. macOS's sandbox forbids process creation.
        #[cfg(target_os = "linux")]
        loop {
            match rustix::process::wait(rustix::process::WaitOptions::empty()) {
                Err(rustix::io::Errno::CHILD) => break,
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(_) => std::thread::sleep(WAIT),
            }
        }
        self.retired = true;
        status
    }
}
impl Drop for ChildGroup {
    fn drop(&mut self) {
        if !self.retired {
            self.retire();
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn rendezvous_namespace_is_private_and_removed_with_owner() {
        let owner = PluginRendezvous::bind().expect("rendezvous");
        let path = owner.directory.path().to_owned();
        assert_eq!(path.parent(), Some(std::path::Path::new("/tmp")));
        assert_eq!(
            std::fs::metadata(&path)
                .expect("private directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(path.join("owner.sock").exists());
        drop(owner);
        assert!(
            !path.exists(),
            "dropping owner removes the socket namespace"
        );
    }
}
