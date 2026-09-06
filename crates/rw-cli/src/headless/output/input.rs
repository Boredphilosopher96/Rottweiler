//! Nonblocking line admission: no input producer may wait for terminal output.
use super::InputLine;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

const INPUT_SLOTS: usize = rw_types::MAX_CLIENT_CONTROLS;
const INPUT_BYTES: usize = rw_types::MAX_CONTROL_RETAINED_BYTES;

#[derive(Default)]
pub(super) struct InputFailure(AtomicU8);

impl InputFailure {
    fn record(&self, kind: u8) {
        let _ = self
            .0
            .compare_exchange(0, kind, Ordering::AcqRel, Ordering::Acquire);
    }
    pub(super) fn message(&self) -> Option<&'static str> {
        match self.0.load(Ordering::Acquire) {
            1 => Some("REPL input queue is full; unsubmitted queued input was refused"),
            2 => Some(
                "REPL input exceeds its retained byte allowance; unsubmitted queued input was refused",
            ),
            _ => None,
        }
    }
}

pub(super) struct InputDelivery {
    pub value: InputLine,
    pub bytes: Option<OwnedSemaphorePermit>,
}

pub(super) struct PendingInput {
    slot: mpsc::OwnedPermit<InputDelivery>,
    delivery: InputDelivery,
}

impl PendingInput {
    pub(super) fn publish(self) {
        self.slot.send(self.delivery);
    }
}

pub(super) struct InputSender {
    sender: mpsc::Sender<InputDelivery>,
    bytes: Arc<Semaphore>,
    pub failure: Arc<InputFailure>,
}

impl InputSender {
    pub(super) fn admit(&self, value: InputLine) -> Option<PendingInput> {
        let heap = match &value {
            InputLine::Line(text) | InputLine::Error(text) => text.capacity(),
            _ => 0,
        };
        let (slot, permit) = self.reserve(heap)?;
        Some(PendingInput {
            slot,
            delivery: InputDelivery {
                value,
                bytes: Some(permit),
            },
        })
    }

    pub(super) fn admit_text(&self, text: &str) -> Option<PendingInput> {
        let (slot, permit) = self.reserve(text.len())?;
        Some(PendingInput {
            slot,
            delivery: InputDelivery {
                value: InputLine::Line(text.to_owned()),
                bytes: Some(permit),
            },
        })
    }

    fn reserve(
        &self,
        heap: usize,
    ) -> Option<(mpsc::OwnedPermit<InputDelivery>, OwnedSemaphorePermit)> {
        let slot = match self.sender.clone().try_reserve_owned() {
            Ok(slot) => slot,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.failure.record(1);
                return None;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return None,
        };
        let count = heap
            .checked_add(std::mem::size_of::<InputDelivery>())
            .and_then(|count| u32::try_from(count).ok());
        let permit = count.and_then(|count| self.bytes.clone().try_acquire_many_owned(count).ok());
        let Some(permit) = permit else {
            self.failure.record(2);
            return None;
        };
        Some((slot, permit))
    }
}

pub(super) struct InputReceiver {
    receiver: mpsc::Receiver<InputDelivery>,
    failure: Arc<InputFailure>,
    refused: bool,
}

impl InputReceiver {
    pub(super) async fn recv(&mut self) -> Option<InputDelivery> {
        if self.refused {
            return None;
        }
        if self.failure.message().is_some() {
            return self.refuse();
        }
        let received = self.receiver.recv().await;
        if self.failure.message().is_some() {
            return self.refuse();
        }
        received
    }

    fn refuse(&mut self) -> Option<InputDelivery> {
        let message = self.failure.message()?;
        self.refused = true;
        self.receiver.close();
        while self.receiver.try_recv().is_ok() {}
        Some(InputDelivery {
            value: InputLine::Error(message.to_owned()),
            bytes: None,
        })
    }
}

pub(super) fn channel() -> (InputSender, InputReceiver) {
    let (sender, receiver) = mpsc::channel(INPUT_SLOTS);
    let failure = Arc::new(InputFailure::default());
    (
        InputSender {
            sender,
            bytes: Arc::new(Semaphore::new(INPUT_BYTES)),
            failure: failure.clone(),
        },
        InputReceiver {
            receiver,
            failure,
            refused: false,
        },
    )
}

#[cfg(test)]
mod tests;
