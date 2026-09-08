//! Synchronous process-group ownership for finite, nonblocking-pipe workers.
use std::{
    io,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use crate::{ResourceClass, ResourceLease, try_acquire};

/// Owns execution admission and the child from spawn through group retirement.
/// Pipe handles may move into the same synchronous worker, never detached tasks.
pub struct BlockingProcess {
    state: Option<State>,
}

struct State {
    child: Child,
    group: Option<rustix::process::Pid>,
    signalled: bool,
    _lease: ResourceLease,
}

/// Pipe custody may move to a worker; reaping authority stays with the owner.
pub struct ProcessPipes {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl BlockingProcess {
    /// Admit and launch a new process group before exposing its pipe handles.
    ///
    /// # Errors
    /// Rejects exhausted admission or a failed spawn.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let lease = try_acquire(ResourceClass::Process).map_err(io::Error::other)?;
        let child = command.process_group(0).spawn()?;
        let group = i32::try_from(child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw);
        Ok(Self {
            state: Some(State {
                child,
                group,
                signalled: false,
                _lease: lease,
            }),
        })
    }

    /// Transfer captured pipes without exposing the child's reaping authority.
    ///
    /// # Errors
    /// Rejects access after retirement.
    pub fn take_pipes(&mut self) -> io::Result<ProcessPipes> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| io::Error::other("process has retired"))?;
        Ok(ProcessPipes {
            stdin: state.child.stdin.take(),
            stdout: state.child.stdout.take(),
            stderr: state.child.stderr.take(),
        })
    }

    /// Return the owned child's identifier for observation, never external reaping.
    ///
    /// # Errors
    /// Rejects access after retirement.
    pub fn id(&self) -> io::Result<u32> {
        self.state
            .as_ref()
            .map(|state| state.child.id())
            .ok_or_else(|| io::Error::other("process has retired"))
    }

    /// Observe exit while leaving the child waitable to anchor final signalling.
    ///
    /// # Errors
    /// Reports failed exit observation or access after retirement.
    pub fn try_status(&self) -> io::Result<Option<ExitStatus>> {
        use rustix::process::{Pid, WaitId, WaitIdOptions};
        use std::os::unix::process::ExitStatusExt;
        let pid = Pid::from_raw(i32::try_from(self.id()?).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("invalid child identifier"))?;
        let Some(status) = rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )?
        else {
            return Ok(None);
        };
        let raw = if let Some(code) = status.exit_status() {
            code << 8
        } else if let Some(signal) = status.terminating_signal() {
            signal | if status.dumped() { 0x80 } else { 0 }
        } else {
            return Err(io::Error::other("unexpected child wait status"));
        };
        Ok(Some(ExitStatus::from_raw(raw)))
    }

    /// Kill remaining group members and wait for terminal proof.
    /// Unknown settlement keeps this worker and its execution capacity occupied.
    /// Call from an owned blocking worker, never an async executor thread.
    pub fn settle(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.signal();
            while !state.retired() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        self.state.take();
    }
}

impl State {
    fn signal(&mut self) {
        if self.signalled {
            return;
        }
        // Never repeat a numeric signal after retirement has begun reaping.
        self.signalled = true;
        if let Some(group) = self.group {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = self.child.kill();
    }

    fn retired(&mut self) -> bool {
        let reaped = matches!(self.child.try_wait(), Ok(Some(_)));
        if let Some(group) = self.group {
            if matches!(
                rustix::process::test_kill_process_group(group),
                Err(rustix::io::Errno::SRCH)
            ) {
                // Retire the numeric identity immediately; later cleanup must
                // not inspect or signal a process that reuses this group id.
                self.group = None;
            } else {
                return false;
            }
        }
        reaped
    }
}

impl Drop for BlockingProcess {
    fn drop(&mut self) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        state.signal();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if state.retired() {
                return;
            }
            if Instant::now() >= deadline {
                // Unwinding cannot report proof that does not exist. Preserve
                // the actual child and group authority with its capacity.
                tracing::error!(
                    pid = state.child.id(),
                    "process settlement unavailable; retaining physical owner"
                );
                Box::leak(Box::new(state));
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
