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

pub(super) async fn run(operation: impl FnOnce(Stop) -> SessionWork) -> Result<()> {
    // Install signal receivers before the owner can start a child process.
    let mut signals = ShutdownSignals::new().into_diagnostic()?;
    let (stop, stopped) = watch::channel(false);
    let caller = Caller(stop);
    let mut task = tokio::spawn(operation(Stop(stopped)).instrument(tracing::Span::current()));
    tokio::select! {
        result = &mut task => result.into_diagnostic()?,
        signal = signals.wait() => {
            drop(caller);
            let settled = task.await.into_diagnostic()?;
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
