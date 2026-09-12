//! Caller cancellation requests settlement from the task that owns a client session.
use miette::{IntoDiagnostic as _, Result};
use std::{future::Future, io, pin::Pin};
use tokio::sync::watch;
use tracing::Instrument as _;

pub(super) struct Stop(watch::Receiver<bool>);
impl Stop {
    pub(super) fn requested(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }
    pub(super) async fn cancelled(&mut self) {
        if !*self.0.borrow_and_update() {
            let _ = self.0.changed().await;
        }
    }
}

struct Caller(watch::Sender<bool>);
impl Drop for Caller {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

type SessionWork = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

struct OwnedSession {
    caller: Caller,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}
impl OwnedSession {
    fn start(operation: impl FnOnce(Stop) -> SessionWork) -> Self {
        let (stop, stopped) = watch::channel(false);
        Self {
            caller: Caller(stop),
            task: Some(tokio::spawn(
                operation(Stop(stopped)).instrument(tracing::Span::current()),
            )),
        }
    }
    async fn wait(&mut self) -> Result<()> {
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        let result = task.await;
        self.task = None;
        result.into_diagnostic()?
    }
    async fn shutdown(&mut self) -> Result<()> {
        let _ = self.caller.0.send(true);
        self.wait().await
    }
}

/// Own server/session resources through cooperative shutdown after caller loss.
/// This entrypoint does not register or consume process signals.
pub(super) async fn own(operation: impl FnOnce(Stop) -> SessionWork) -> Result<()> {
    OwnedSession::start(operation).wait().await
}

pub(super) async fn run(operation: impl FnOnce(Stop) -> SessionWork) -> Result<()> {
    // Install signal receivers before the owner can start a child process.
    let mut signals = ShutdownSignals::new().into_diagnostic()?;
    let mut owner = OwnedSession::start(operation);
    tokio::select! {
        result = owner.wait() => result,
        signal = signals.wait() => {
            let settled = owner.shutdown().await;
            signal.into_diagnostic().and(settled)
        }
    }
}

struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}
impl ShutdownSignals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            hangup: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?,
        })
    }
    async fn wait(&mut self) -> io::Result<()> {
        tokio::select! {
            _ = self.interrupt.recv() => Ok(()),
            _ = self.terminate.recv() => Ok(()),
            _ = self.hangup.recv() => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
