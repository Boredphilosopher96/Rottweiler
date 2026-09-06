#![allow(clippy::expect_used)]
use super::{HOST_EVENT_STALL_TIMEOUT, HostEvent, mpsc, send_result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct RetainedFrame(Arc<AtomicBool>);
impl AsRef<[u8]> for RetainedFrame {
    fn as_ref(&self) -> &[u8] {
        b"frame"
    }
}
impl Drop for RetainedFrame {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn event(dropped: &Arc<AtomicBool>) -> HostEvent {
    HostEvent {
        json: bytes::Bytes::from_owner(RetainedFrame(Arc::clone(dropped))),
        sequence: None,
    }
}

#[tokio::test(start_paused = true)]
async fn full_transport_retires_at_deadline_and_preserves_queued_frame_ownership() {
    let (send, mut receive) = mpsc::channel(1);
    let first = Arc::new(AtomicBool::new(false));
    let second = Arc::new(AtomicBool::new(false));
    send.send(Ok(event(&first))).await.expect("full transport");
    let pending = event(&second);
    let started = tokio::time::Instant::now();
    let task = tokio::spawn(async move { send_result(&send, Ok(pending)).await });
    assert!(!task.await.expect("forwarder settled"));
    assert_eq!(started.elapsed(), HOST_EVENT_STALL_TIMEOUT);
    assert!(second.load(Ordering::Acquire));
    assert!(!first.load(Ordering::Acquire));
    assert!(
        receive.is_closed(),
        "stalled stream closes without pretending to deliver terminal data"
    );
    let queued = receive.recv().await.expect("queued frame").expect("frame");
    assert_eq!(&queued.json[..], b"frame");
    assert!(receive.recv().await.is_none());
    drop(queued);
    assert!(first.load(Ordering::Acquire));
}

#[tokio::test(start_paused = true)]
async fn transport_receiver_loss_releases_pending_frame_without_waiting_for_deadline() {
    let (send, receive) = mpsc::channel(1);
    let queued = Arc::new(AtomicBool::new(false));
    send.send(Ok(event(&queued))).await.expect("full transport");
    let pending = Arc::new(AtomicBool::new(false));
    let frame = event(&pending);
    let task = tokio::spawn(async move { send_result(&send, Ok(frame)).await });
    tokio::task::yield_now().await;
    assert!(!pending.load(Ordering::Acquire));
    drop(receive);
    assert!(!task.await.expect("closed forwarder"));
    assert!(pending.load(Ordering::Acquire));
    assert!(queued.load(Ordering::Acquire));
}
