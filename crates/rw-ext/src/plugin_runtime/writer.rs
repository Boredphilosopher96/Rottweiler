//! Bounded control priority without reordering a producer's acknowledged data.
use std::sync::Arc;

use rw_plugin_protocol::{
    CONTROL_QUEUE_BYTES, CONTROL_QUEUE_FRAMES, DATA_QUEUE_BYTES, MAX_FRAME_BYTES, RpcFrame,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

#[derive(Clone)]
pub(super) struct RpcWriter {
    control: mpsc::Sender<QueuedFrame>,
    data: mpsc::Sender<QueuedFrame>,
    control_bytes: Arc<Semaphore>,
    data_bytes: Arc<Semaphore>,
    #[cfg(test)]
    encodings: Arc<std::sync::atomic::AtomicUsize>,
}

pub(super) struct RpcReceiver {
    control: mpsc::Receiver<QueuedFrame>,
    data: mpsc::Receiver<QueuedFrame>,
}

pub(super) struct QueuedFrame {
    pub(super) bytes: Vec<u8>,
    _permit: OwnedSemaphorePermit,
    written: Option<oneshot::Sender<()>>,
}

impl QueuedFrame {
    pub(super) fn complete(self) {
        if let Some(written) = self.written {
            let _ = written.send(());
        }
    }
}

struct PreparedFrame {
    frame: RpcFrame,
    bytes: usize,
}

impl PreparedFrame {
    fn new(frame: RpcFrame) -> Result<Self, ()> {
        frame.validate().map_err(|_| ())?;
        let mut size = FrameSize(0);
        serde_json::to_writer(&mut size, &frame).map_err(|_| ())?;
        Ok(Self {
            frame,
            bytes: size.0 + 1,
        })
    }
}

struct FrameSize(usize);

impl std::io::Write for FrameSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_FRAME_BYTES {
            return Err(std::io::Error::other("plugin frame exceeds byte limit"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const _: () = assert!(CONTROL_QUEUE_BYTES > MAX_FRAME_BYTES);
const _: () = assert!(DATA_QUEUE_BYTES > MAX_FRAME_BYTES);

impl RpcWriter {
    pub(super) fn channel() -> (Self, RpcReceiver) {
        let (control, control_rx) = mpsc::channel(CONTROL_QUEUE_FRAMES);
        let (data, data_rx) = mpsc::channel(CONTROL_QUEUE_FRAMES);
        (
            Self {
                control,
                data,
                control_bytes: Arc::new(Semaphore::new(CONTROL_QUEUE_BYTES)),
                data_bytes: Arc::new(Semaphore::new(DATA_QUEUE_BYTES)),
                #[cfg(test)]
                encodings: Arc::default(),
            },
            RpcReceiver {
                control: control_rx,
                data: data_rx,
            },
        )
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn encode(&self, prepared: PreparedFrame) -> Result<Vec<u8>, ()> {
        #[cfg(test)]
        self.encodings
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut bytes = Vec::with_capacity(prepared.bytes);
        serde_json::to_writer(&mut bytes, &prepared.frame).map_err(|_| ())?;
        bytes.push(b'\n');
        drop(prepared);
        Ok(bytes)
    }

    pub(super) async fn send(&self, frame: RpcFrame) -> Result<(), ()> {
        let prepared = PreparedFrame::new(frame)?;
        let count = u32::try_from(prepared.bytes).map_err(|_| ())?;
        let permit = Arc::clone(&self.control_bytes)
            .acquire_many_owned(count)
            .await
            .map_err(|_| ())?;
        let bytes = self.encode(prepared)?;
        self.control
            .send(QueuedFrame {
                bytes,
                _permit: permit,
                written: None,
            })
            .await
            .map_err(|_| ())
    }

    /// Waits for the actual pipe write so its producer's terminal control frame
    /// cannot overtake earlier body data when control traffic has priority.
    pub(super) async fn send_data(&self, frame: RpcFrame) -> Result<(), ()> {
        let prepared = PreparedFrame::new(frame)?;
        let count = u32::try_from(prepared.bytes).map_err(|_| ())?;
        let permit = Arc::clone(&self.data_bytes)
            .acquire_many_owned(count)
            .await
            .map_err(|_| ())?;
        let bytes = self.encode(prepared)?;
        let (written, receiver) = oneshot::channel();
        self.data
            .send(QueuedFrame {
                bytes,
                _permit: permit,
                written: Some(written),
            })
            .await
            .map_err(|_| ())?;
        receiver.await.map_err(|_| ())
    }
}

impl RpcReceiver {
    pub(super) async fn recv(&mut self) -> Option<QueuedFrame> {
        tokio::select! {
            biased;
            Some(frame) = self.control.recv() => Some(frame),
            Some(frame) = self.data.recv() => Some(frame),
            else => None,
        }
    }

    #[cfg(test)]
    pub(super) async fn recv_frame(&mut self) -> Option<RpcFrame> {
        let frame = self.recv().await?;
        let decoded = serde_json::from_slice(&frame.bytes).ok();
        frame.complete();
        decoded
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use rw_plugin_protocol::{RpcId, RpcNotification, RpcSuccess};
    use serde_json::json;

    fn data() -> RpcFrame {
        RpcFrame::Notification(RpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: "provider/http_event".to_owned(),
            params: Some(json!({"event":"body"})),
        })
    }
    fn response(id: i64) -> RpcFrame {
        RpcFrame::Success(RpcSuccess {
            jsonrpc: "2.0".to_owned(),
            id: RpcId::Number(id),
            result: json!(null),
        })
    }

    #[tokio::test]
    async fn saturated_byte_admission_never_allocates_an_encoded_pending_frame() {
        use std::sync::atomic::Ordering;
        let (writer, mut receiver) = RpcWriter::channel();
        let control = writer
            .control_bytes
            .clone()
            .acquire_many_owned(u32::try_from(CONTROL_QUEUE_BYTES).unwrap())
            .await
            .unwrap();
        let data = writer
            .data_bytes
            .clone()
            .acquire_many_owned(u32::try_from(DATA_QUEUE_BYTES).unwrap())
            .await
            .unwrap();
        let mut pending_control = Box::pin(writer.send(response(1)));
        let mut pending_data = Box::pin(writer.send_data(response(2)));
        assert!(futures_util::poll!(&mut pending_control).is_pending());
        assert!(futures_util::poll!(&mut pending_data).is_pending());
        assert_eq!(writer.encodings.load(Ordering::Relaxed), 0);
        drop(control);
        pending_control.await.unwrap();
        let active = receiver.recv().await.unwrap();
        assert_eq!(
            writer.control_bytes.available_permits(),
            CONTROL_QUEUE_BYTES - active.bytes.len()
        );
        active.complete();
        assert_eq!(
            writer.control_bytes.available_permits(),
            CONTROL_QUEUE_BYTES
        );
        drop(data);
        assert!(futures_util::poll!(&mut pending_data).is_pending());
        let active = receiver.recv().await.unwrap();
        assert_eq!(
            writer.data_bytes.available_permits(),
            DATA_QUEUE_BYTES - active.bytes.len()
        );
        active.complete();
        pending_data.await.unwrap();
        assert_eq!(writer.data_bytes.available_permits(), DATA_QUEUE_BYTES);
    }

    #[tokio::test]
    async fn control_overtakes_other_data_but_terminal_waits_for_its_body_write() {
        let (writer, mut receiver) = RpcWriter::channel();
        let producer = tokio::spawn({
            let writer = writer.clone();
            async move {
                writer.send_data(data()).await.expect("body write");
                writer.send(response(1)).await.expect("terminal enqueue");
            }
        });
        while receiver.data.is_empty() {
            tokio::task::yield_now().await;
        }
        writer.send(response(2)).await.expect("unrelated control");
        assert_eq!(receiver.recv_frame().await, Some(response(2)));
        assert!(
            receiver.control.is_empty(),
            "terminal cannot precede body write acknowledgement"
        );
        assert_eq!(receiver.recv_frame().await, Some(data()));
        producer.await.expect("producer");
        assert_eq!(receiver.recv_frame().await, Some(response(1)));
    }
}
