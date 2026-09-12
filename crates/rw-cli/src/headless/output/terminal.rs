//! One bounded physical owner for headless terminal input, output and shutdown.
mod io;
mod lines;
mod output_only;
mod worker;

use super::{
    MAX_REPL_OUTPUT_BYTES,
    input::{self, InputFailure, InputReceiver},
};
use miette::{IntoDiagnostic, Result, miette};
use std::{
    io::{self as std_io, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};

struct Wake {
    writer: UnixStream,
    cancelled: AtomicBool,
}
impl Wake {
    fn notify(&self) {
        let _ = (&self.writer).write(&[1]);
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify();
    }
}
enum Descriptors {
    Interactive { input: OwnedFd, output: OwnedFd },
    Output { stdout: OwnedFd, stderr: OwnedFd },
}
struct OutputRequest {
    message: String,
    offset: usize,
    stderr: bool,
    done: oneshot::Sender<std_io::Result<()>>,
    _slot: OwnedSemaphorePermit,
}

pub(super) struct Interrupts {
    receiver: watch::Receiver<()>,
}
impl Interrupts {
    pub(super) async fn recv(&mut self) -> std_io::Result<()> {
        tokio::select! {
            changed = self.receiver.changed() => {
                if changed.is_err() { std::future::pending::<()>().await; }
                drop(self.receiver.borrow_and_update());
                Ok(())
            },
            signal = tokio::signal::ctrl_c() => signal,
        }
    }
}

pub(super) struct Terminal {
    output: mpsc::SyncSender<OutputRequest>,
    output_slot: Arc<Semaphore>,
    wake: Arc<Wake>,
    failure: Arc<InputFailure>,
    finished: Option<oneshot::Receiver<std_io::Result<()>>>,
}
impl Terminal {
    pub(super) async fn start() -> Result<(InputReceiver, Interrupts, Self)> {
        let active = Self::admit()?;
        let input = io::duplicate(std_io::stdin()).into_diagnostic()?;
        let output = io::duplicate(std_io::stdout()).into_diagnostic()?;
        Self::spawn(input, output, active).await
    }
    fn admit() -> Result<OwnedSemaphorePermit> {
        static ACTIVE: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let active = ACTIVE
            .get_or_init(|| Arc::new(Semaphore::new(1)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| miette!("headless terminal is still active"))?;
        Ok(active)
    }
    pub(super) async fn start_output() -> Result<Self> {
        let active = Self::admit()?;
        let stdout = io::duplicate(std_io::stdout()).into_diagnostic()?;
        let stderr = io::duplicate(std_io::stderr()).into_diagnostic()?;
        let (_, _, terminal) =
            Self::spawn_mode(Descriptors::Output { stdout, stderr }, active, || {}).await?;
        Ok(terminal)
    }
    async fn spawn(
        input: OwnedFd,
        output: OwnedFd,
        active: OwnedSemaphorePermit,
    ) -> Result<(InputReceiver, Interrupts, Self)> {
        Self::spawn_with_finalizer(input, output, active, || {}).await
    }

    async fn spawn_with_finalizer(
        input: OwnedFd,
        output: OwnedFd,
        active: OwnedSemaphorePermit,
        finalize: impl FnOnce() + Send + 'static,
    ) -> Result<(InputReceiver, Interrupts, Self)> {
        Self::spawn_mode(Descriptors::Interactive { input, output }, active, finalize).await
    }

    async fn spawn_mode(
        descriptors: Descriptors,
        active: OwnedSemaphorePermit,
        finalize: impl FnOnce() + Send + 'static,
    ) -> Result<(InputReceiver, Interrupts, Self)> {
        let lease = rw_resources::acquire(
            rw_resources::ResourceClass::Blocking,
            std::future::pending(),
        )
        .await
        .into_diagnostic()?;
        let (reader, writer) = UnixStream::pair().into_diagnostic()?;
        reader.set_nonblocking(true).into_diagnostic()?;
        writer.set_nonblocking(true).into_diagnostic()?;
        let wake = Arc::new(Wake {
            writer,
            cancelled: AtomicBool::new(false),
        });
        let physical_wake = wake.clone();
        let (send, receive) = input::channel();
        let (interrupt_send, interrupt_receive) = watch::channel(());
        let failure = send.failure.clone();
        let (output_sender, requests) = mpsc::sync_channel(1);
        let (done, finished) = oneshot::channel();
        std::thread::Builder::new()
            .name("rw-terminal".to_owned())
            .spawn(move || {
                let result = {
                    let _lease = lease;
                    let _active = active;
                    let result = match descriptors {
                        Descriptors::Interactive { input, output } => worker::run(
                            input,
                            output,
                            reader,
                            &physical_wake,
                            &send,
                            &requests,
                            &interrupt_send,
                        ),
                        Descriptors::Output { stdout, stderr } => {
                            output_only::run(stdout, stderr, reader, &physical_wake, &requests)
                        }
                    };
                    finalize();
                    drop(requests);
                    drop(send);
                    result
                };
                let _ = done.send(result);
            })
            .into_diagnostic()?;
        Ok((
            receive,
            Interrupts {
                receiver: interrupt_receive,
            },
            Self {
                output: output_sender,
                output_slot: Arc::new(Semaphore::new(1)),
                wake,
                failure,
                finished: Some(finished),
            },
        ))
    }
    pub(super) async fn print(&mut self, message: String) -> Result<()> {
        self.print_to(message, false).await
    }
    pub(super) async fn print_to(&mut self, message: String, stderr: bool) -> Result<()> {
        if let Some(failure) = self.failure.message() {
            return Err(miette!(failure));
        }
        if message.capacity() > MAX_REPL_OUTPUT_BYTES {
            return Err(miette!(
                "headless output exceeds its retained byte allowance"
            ));
        }
        let slot = self
            .output_slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| miette!("headless output is still owned by a physical write"))?;
        let (done, result) = oneshot::channel();
        self.output
            .try_send(OutputRequest {
                message,
                offset: 0,
                stderr,
                done,
                _slot: slot,
            })
            .map_err(|_| miette!("headless output worker is unavailable"))?;
        self.wake.notify();
        let result = result.await;
        if let Some(failure) = self.failure.message() {
            return Err(miette!(failure));
        }
        result
            .map_err(|_| miette!("headless terminal output failed"))?
            .into_diagnostic()
    }
    /// Request physical settlement immediately, independently of actor admission.
    /// The request bytes and worker permits remain owned until `close` proves it.
    pub(super) fn cancel(&self) {
        self.wake.cancel();
    }
    pub(super) async fn close(&mut self) -> Result<()> {
        self.cancel();
        if let Some(finished) = self.finished.as_mut() {
            let result = finished.await;
            self.finished = None;
            result.into_diagnostic()?.into_diagnostic()?;
        }
        Ok(())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.wake.cancel();
    }
}

#[cfg(test)]
mod tests;
