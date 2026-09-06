#![allow(clippy::expect_used)]
use super::*;

#[tokio::test]
async fn input_refusal_is_nonblocking_and_retires_unsubmitted_backlog() {
    let (send, mut receive) = channel();
    for _ in 0..INPUT_SLOTS {
        send.admit(InputLine::Line("queued".to_owned()))
            .expect("slot")
            .publish();
    }
    assert!(send.admit(InputLine::Line("refused".to_owned())).is_none());
    assert!(
        matches!(receive.recv().await.expect("explicit refusal").value, InputLine::Error(message) if message.contains("queue is full"))
    );
    assert_eq!(send.bytes.available_permits(), INPUT_BYTES);
    assert!(receive.recv().await.is_none());
}

#[tokio::test]
async fn input_credits_follow_delivery_and_cancelled_receiver_preserves_input() {
    let (send, mut receive) = channel();
    let pending = send
        .admit(InputLine::Line("first".to_owned()))
        .expect("admit");
    let charged = INPUT_BYTES - send.bytes.available_permits();
    pending.publish();
    let delivered = receive.recv().await.expect("first");
    assert_eq!(INPUT_BYTES - send.bytes.available_permits(), charged);
    assert!(matches!(&delivered.value, InputLine::Line(text) if text == "first"));
    drop(delivered);
    assert_eq!(send.bytes.available_permits(), INPUT_BYTES);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1), receive.recv())
            .await
            .is_err()
    );
    send.admit(InputLine::Line("next".to_owned()))
        .expect("next slot")
        .publish();
    assert!(
        matches!(receive.recv().await.expect("not consumed by cancellation").value, InputLine::Line(text) if text == "next")
    );
}

#[tokio::test]
async fn input_capacity_refusal_precedes_publication() {
    let (send, mut receive) = channel();
    assert!(
        send.admit(InputLine::Line(String::with_capacity(INPUT_BYTES + 1)))
            .is_none()
    );
    assert!(
        matches!(receive.recv().await.expect("explicit refusal").value, InputLine::Error(message) if message.contains("byte allowance"))
    );
    assert_eq!(send.bytes.available_permits(), INPUT_BYTES);
}

#[tokio::test]
async fn copied_input_admits_before_allocation_and_holds_exact_credit() {
    let (send, mut receive) = channel();
    let text = "small UTF-8 界";
    let pending = send.admit_text(text).expect("admitted before copy");
    assert_eq!(
        INPUT_BYTES - send.bytes.available_permits(),
        text.len() + std::mem::size_of::<InputDelivery>()
    );
    pending.publish();
    let delivered = receive.recv().await.expect("delivered");
    assert_eq!(
        INPUT_BYTES - send.bytes.available_permits(),
        text.len() + std::mem::size_of::<InputDelivery>()
    );
    drop(delivered);
    assert_eq!(send.bytes.available_permits(), INPUT_BYTES);
    let held = send
        .bytes
        .clone()
        .try_acquire_many_owned(u32::try_from(INPUT_BYTES).expect("bounded bytes"))
        .expect("all credit");
    assert!(send.admit_text(text).is_none());
    assert_eq!(send.sender.capacity(), INPUT_SLOTS);
    drop(held);
    assert!(matches!(
        receive.recv().await.expect("refusal").value,
        InputLine::Error(_)
    ));
}
