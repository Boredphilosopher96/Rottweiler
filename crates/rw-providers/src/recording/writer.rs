//! Serialized recording writes and their explicit settlement barrier.
use super::{ProviderError, ProviderErrorKind, WriteJob};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

pub(super) enum WriterMessage {
    Fixture(Box<WriteJob>),
    Barrier(oneshot::Sender<Result<(), ProviderError>>),
    Settled(oneshot::Sender<()>),
}

pub(super) struct RecordingWriter {
    sender: mpsc::Sender<WriterMessage>,
    receiver: Mutex<Option<mpsc::Receiver<WriterMessage>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    failed: Arc<AtomicBool>,
    quarantined: Arc<Mutex<Vec<WriteJob>>>,
}

impl RecordingWriter {
    pub(super) fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
            worker: Mutex::new(None),
            failed: Arc::new(AtomicBool::new(false)),
            quarantined: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn start(&self) {
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(mut receiver) = receiver else {
            return;
        };
        let failed = Arc::clone(&self.failed);
        let quarantined = Arc::clone(&self.quarantined);
        let worker = tokio::task::spawn_blocking(move || {
            let mut first_error = None;
            while let Some(message) = receiver.blocking_recv() {
                match message {
                    WriterMessage::Fixture(mut job) => {
                        let result = if failed.load(Ordering::Acquire) {
                            Err(unsettled())
                        } else {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.write()))
                                .unwrap_or_else(|_| {
                                    failed.store(true, Ordering::Release);
                                    Err(unsettled())
                                })
                        };
                        if first_error.is_none() {
                            first_error = result.as_ref().err().cloned();
                        }
                        if let Some(completion) = job.completion.take() {
                            let _ = completion.send(result);
                        }
                        if failed.load(Ordering::Acquire) {
                            receiver.close();
                            quarantined
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(*job);
                        }
                    }
                    WriterMessage::Settled(completion) => {
                        let _ = completion.send(());
                    }
                    WriterMessage::Barrier(completion) => {
                        let result = first_error.take().map_or(Ok(()), Err);
                        let _ = completion.send(result);
                    }
                }
            }
        });
        *self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(worker);
    }

    pub(super) async fn reserve(&self) -> Result<mpsc::OwnedPermit<WriterMessage>, ProviderError> {
        self.start();
        if self.failed.load(Ordering::Acquire) {
            return Err(unsettled());
        }
        self.sender
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| self.unavailable_error())
    }

    pub(super) async fn settle(&self) -> Result<(), ProviderError> {
        self.start();
        if self.failed.load(Ordering::Acquire) {
            return Err(unsettled());
        }
        let proof = async {
            let (completion, result) = oneshot::channel();
            self.sender
                .send(WriterMessage::Settled(completion))
                .await
                .map_err(|_| self.settlement_error())?;
            result.await.map_err(|_| self.settlement_error())?;
            if self.failed.load(Ordering::Acquire) {
                return Err(unsettled());
            }
            Ok(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), proof)
            .await
            .unwrap_or_else(|_| Err(self.settlement_error()))
    }

    pub(super) async fn flush(&self) -> Result<(), ProviderError> {
        self.start();
        let (completion, result) = oneshot::channel();
        self.sender
            .send(WriterMessage::Barrier(completion))
            .await
            .map_err(|_| self.unavailable_error())?;
        result.await.map_err(|_| self.unavailable_error())?
    }

    fn settlement_error(&self) -> ProviderError {
        self.failed.store(true, Ordering::Release);
        unsettled()
    }

    fn unavailable_error(&self) -> ProviderError {
        let worker_finished = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(JoinHandle::is_finished);
        ProviderError::new(
            ProviderErrorKind::Protocol,
            if worker_finished {
                "replay fixture writer stopped unexpectedly"
            } else {
                "replay fixture writer is unavailable"
            },
        )
    }
}

fn unsettled() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::EffectsUnsettled,
        "recording worker did not prove effect settlement",
    )
}

#[cfg(test)]
mod tests;
