//! A PTY wait owns the actual child, IO threads, and process admission through proof.
use super::TerminalModeGuard;
use futures_util::{
    FutureExt as _,
    future::{BoxFuture, Shared},
};
use rustix::process::{Pid, Signal};
use rw_resources::ResourceLease;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

type Completion = Shared<BoxFuture<'static, Result<i32, WaitFailure>>>;
#[derive(Clone, Debug)]
struct WaitFailure {
    message: Arc<str>,
    unsettled: bool,
}
impl std::fmt::Display for WaitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for WaitFailure {}
fn failure(message: impl Into<Arc<str>>, unsettled: bool) -> WaitFailure {
    WaitFailure {
        message: message.into(),
        unsettled,
    }
}
pub(super) fn unsettled(message: impl Into<Arc<str>>) -> io::Error {
    io::Error::other(failure(message, true))
}
pub(super) fn is_unsettled(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|error| error.downcast_ref::<WaitFailure>())
        .is_some_and(|failure| failure.unsettled)
}
const RETIREMENT_PROOF_DEADLINE: Duration = Duration::from_secs(5);
const HANGUP_GRACE: Duration = Duration::from_millis(100);

pub(super) struct ProcessOwner {
    physical: Option<Physical>,
    completion: Option<Completion>,
    cancelled: Arc<AtomicBool>,
}
pub(super) struct Physical {
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub group: Option<Pid>,
    pub group_known: bool,
    pub cancelled: Arc<AtomicBool>,
    pub active: Arc<Mutex<bool>>,
    pub terminal_mode: Arc<Mutex<Option<TerminalModeGuard>>>,
    pub input: Option<thread::JoinHandle<io::Result<()>>>,
    pub output: Option<thread::JoinHandle<io::Result<()>>>,
    pub idle_writer: Option<Box<dyn io::Write + Send>>,
    credit: Option<ResourceLease>,
    status: Option<i32>,
    stopping: Option<tokio::time::Instant>,
    killed: bool,
    error: Option<String>,
}
impl ProcessOwner {
    pub fn new(child: Box<dyn portable_pty::Child + Send + Sync>, credit: ResourceLease) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let group = child
            .process_id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw);
        Self {
            physical: Some(Physical {
                child,
                group,
                group_known: group.is_some(),
                cancelled: cancelled.clone(),
                active: Arc::new(Mutex::new(true)),
                terminal_mode: Arc::new(Mutex::new(None)),
                input: None,
                output: None,
                idle_writer: None,
                credit: Some(credit),
                status: None,
                stopping: None,
                killed: false,
                error: None,
            }),
            completion: None,
            cancelled,
        }
    }
    pub fn physical(&mut self) -> io::Result<&mut Physical> {
        self.physical
            .as_mut()
            .ok_or_else(|| io::Error::other("PTY ownership already transferred"))
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    fn start(&mut self) -> io::Result<()> {
        let Some(physical) = self.physical.take() else {
            return Ok(());
        };
        // This guard also owns a task which is dropped before its first poll.
        let retirement = Retirement(Some(physical));
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|error| unsettled(error.to_string()))?;
        let (send, receive) = tokio::sync::oneshot::channel();
        self.completion = Some(
            async move {
                receive
                    .await
                    .unwrap_or_else(|_| Err(failure("PTY owner lost settlement proof", true)))
            }
            .boxed()
            .shared(),
        );
        runtime.spawn(retirement.run(send));
        Ok(())
    }
    pub async fn wait(&mut self) -> io::Result<i32> {
        self.start()?;
        self.completion
            .as_ref()
            .ok_or_else(|| unsettled("PTY completion unavailable"))?
            .clone()
            .await
            .map_err(io::Error::other)
    }
}
impl Drop for ProcessOwner {
    fn drop(&mut self) {
        self.cancel();
        let _ = self.start();
    }
}

pub(super) fn restore_terminal(mode: &Mutex<Option<TerminalModeGuard>>) -> io::Result<()> {
    if let Some(mut guard) = mode
        .lock()
        .map_err(|_| io::Error::other("terminal mode owner poisoned"))?
        .take()
    {
        guard.restore()?;
    }
    Ok(())
}
impl Physical {
    fn signal(&mut self, signal: Signal) -> io::Result<()> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| io::Error::other("PTY signal owner poisoned"))?;
        let group = if let Some(group) = self.group {
            match rustix::process::kill_process_group(group, signal) {
                Ok(()) => Ok(()),
                Err(rustix::io::Errno::SRCH) => {
                    self.group = None;
                    *active = false;
                    Ok(())
                }
                Err(error) => Err(io::Error::from(error)),
            }
        } else {
            Ok(())
        };
        let child = if signal == Signal::KILL && self.status.is_none() {
            self.child.kill()
        } else {
            Ok(())
        };
        group.and(child)
    }
    fn begin_stop(&mut self, kill: bool) -> io::Result<()> {
        self.stopping.get_or_insert_with(tokio::time::Instant::now);
        self.cancelled.store(true, Ordering::Release);
        self.idle_writer.take();
        if let Err(error) = restore_terminal(&self.terminal_mode) {
            self.error.get_or_insert(error.to_string());
        }
        if kill {
            if !self.killed {
                self.killed = true;
                self.signal(Signal::KILL)?;
            }
        } else {
            self.signal(Signal::HUP)?;
        }
        Ok(())
    }
    fn join_finished(&mut self) {
        for (handle, name) in [
            (&mut self.input, "PTY input"),
            (&mut self.output, "PTY output"),
        ] {
            if handle.as_ref().is_some_and(thread::JoinHandle::is_finished)
                && let Some(handle) = handle.take()
            {
                let result = handle
                    .join()
                    .map_err(|_| io::Error::other(format!("{name} thread panicked")))
                    .and_then(|value| value);
                if let Err(error) = result {
                    self.error.get_or_insert(error.to_string());
                }
            }
        }
    }
    fn poll(&mut self) -> io::Result<bool> {
        if !self.group_known {
            return Err(io::Error::other("PTY process group identity unavailable"));
        }
        if self.cancelled.load(Ordering::Acquire) && self.stopping.is_none() {
            self.begin_stop(true)?;
        }
        if self.status.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.status = Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX));
            if self.stopping.is_none() {
                self.begin_stop(false)?;
            }
        }
        if self.stopping.is_some() {
            let mut active = self
                .active
                .lock()
                .map_err(|_| io::Error::other("PTY signal owner poisoned"))?;
            if let Some(group) = self.group {
                match rustix::process::test_kill_process_group(group) {
                    Err(rustix::io::Errno::SRCH) => {
                        self.group = None;
                    }
                    Ok(()) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if self.group.is_none() {
                *active = false;
            }
            drop(active);
            if !self.killed && self.stopping.is_some_and(|at| at.elapsed() >= HANGUP_GRACE) {
                self.begin_stop(true)?;
            }
        }
        self.join_finished();
        if self.status.is_some()
            && self.group.is_none()
            && self.input.is_none()
            && self.output.is_none()
        {
            *self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
            self.credit.take();
            return Ok(true);
        }
        Ok(false)
    }
}
struct Retirement(Option<Physical>);
impl Retirement {
    async fn run(mut self, send: tokio::sync::oneshot::Sender<Result<i32, WaitFailure>>) {
        let mut send = Some(send);
        while let Some(owner) = self.0.as_mut() {
            match owner.poll() {
                Ok(true) => {
                    let result = owner.error.as_ref().map_or_else(
                        || {
                            owner
                                .status
                                .ok_or_else(|| failure("PTY exit status unavailable", true))
                        },
                        |error| Err(failure(error.as_str(), false)),
                    );
                    self.0.take();
                    if let Some(send) = send.take() {
                        let _ = send.send(result);
                    }
                    return;
                }
                Err(error) => {
                    if let Some(send) = send.take() {
                        let _ = send.send(Err(failure(error.to_string(), true)));
                    }
                    return;
                }
                Ok(false) => {}
            }
            if owner
                .stopping
                .is_some_and(|at| at.elapsed() >= RETIREMENT_PROOF_DEADLINE)
                && let Some(send) = send.take()
            {
                let _ = send.send(Err(failure(
                    "PTY effects unsettled: retirement proof deadline expired",
                    true,
                )));
            }
            // Reporting a deadline does not abandon eventual child/thread proof.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
impl Drop for Retirement {
    fn drop(&mut self) {
        if let Some(mut owner) = self.0.take() {
            let _ = owner.begin_stop(true);
            // Failed wait, missing runtime, panic, or aborted owner: retain actual
            // child, IO handles, terminal authority and charged process capacity.
            std::mem::forget(owner);
        }
    }
}

#[cfg(test)]
mod tests;
