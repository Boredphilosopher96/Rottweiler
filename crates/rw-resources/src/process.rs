//! Synchronous process-group ownership for finite, nonblocking-pipe workers.
use std::{
    io,
    process::{Child, Command},
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
    _lease: ResourceLease,
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
                _lease: lease,
            }),
        })
    }

    /// Access pipes and poll the child while its group remains owned.
    ///
    /// # Errors
    /// Rejects access after retirement.
    pub fn child_mut(&mut self) -> io::Result<&mut Child> {
        self.state
            .as_mut()
            .map(|state| &mut state.child)
            .ok_or_else(|| io::Error::other("process has retired"))
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
