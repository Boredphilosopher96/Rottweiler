//! Print mode owns only output descriptors. It never opens or configures stdin.
use super::{OutputRequest, Wake, worker};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    io::{self, Read as _},
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

struct OutputFd {
    fd: OwnedFd,
    flags: OFlags,
}
impl OutputFd {
    fn open(fd: OwnedFd) -> io::Result<Self> {
        let flags = fcntl_getfl(&fd)?;
        let owner = Self { fd, flags };
        fcntl_setfl(&owner.fd, flags | OFlags::NONBLOCK)?;
        Ok(owner)
    }
    fn restore(&self) -> io::Result<()> {
        fcntl_setfl(&self.fd, self.flags).map_err(Into::into)
    }
}
impl Drop for OutputFd {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub(super) fn run(
    stdout: OwnedFd,
    stderr: OwnedFd,
    mut wake_fd: UnixStream,
    wake: &Wake,
    requests: &mpsc::Receiver<OutputRequest>,
) -> io::Result<()> {
    // Read both original flag sets before changing either: stdout/stderr may
    // refer to the same open file description (shell `2>&1`).
    let stdout_flags = fcntl_getfl(&stdout)?;
    let stderr_flags = fcntl_getfl(&stderr)?;
    let stdout = OutputFd::open(stdout)?;
    let stderr = OutputFd {
        fd: stderr,
        flags: stderr_flags,
    };
    fcntl_setfl(&stderr.fd, stderr.flags | OFlags::NONBLOCK)?;
    let result = pump(&stdout, &stderr, &mut wake_fd, wake, requests);
    // Explicit settlement reports restoration failure before releasing owner.
    let out = fcntl_setfl(&stdout.fd, stdout_flags).map_err(io::Error::from);
    let err = stderr.restore();
    result.and(out).and(err)
}

fn pump(
    stdout: &OutputFd,
    stderr: &OutputFd,
    wake_fd: &mut UnixStream,
    wake: &Wake,
    requests: &mpsc::Receiver<OutputRequest>,
) -> io::Result<()> {
    let mut pending: Option<OutputRequest> = None;
    let mut progress = Instant::now();
    loop {
        if wake.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if pending.is_none() {
            pending = requests.try_recv().ok();
            progress = Instant::now();
        }
        if pending.as_ref().is_some_and(|p| p.message.is_empty()) {
            worker::finish(&mut pending);
        }
        if pending.is_some() && progress.elapsed() >= Duration::from_secs(5) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "print output remained blocked",
            ));
        }
        let destination = if pending.as_ref().is_some_and(|p| p.stderr) {
            stderr
        } else {
            stdout
        };
        let mut pollers = [
            PollFd::new(wake_fd.as_fd(), PollFlags::POLLIN),
            PollFd::new(
                destination.fd.as_fd(),
                if pending.is_some() {
                    PollFlags::POLLOUT
                } else {
                    PollFlags::empty()
                },
            ),
        ];
        match poll(&mut pollers, PollTimeout::from(100_u16)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(io::Error::other(error)),
        }
        let [wake_ready, output_ready] =
            pollers.map(|p| p.revents().unwrap_or_else(PollFlags::empty));
        if wake_ready.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            let mut bytes = [0; 64];
            while wake_fd.read(&mut bytes).is_ok_and(|n| n != 0) {}
            if wake.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        if output_ready.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "print output closed",
            ));
        }
        if output_ready.contains(PollFlags::POLLOUT)
            && let Some(request) = &mut pending
        {
            let count = worker::write_some(
                &destination.fd,
                &request.message.as_bytes()[request.offset..],
            )?;
            request.offset += count;
            if count > 0 {
                progress = Instant::now();
            }
            if request.offset == request.message.len() {
                worker::finish(&mut pending);
            }
        }
    }
}
