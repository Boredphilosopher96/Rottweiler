#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{JournalCommits, MAX_BATCHES, failure};
use rw_core::SESSION_EVENT_VERSION;
use rw_core::{AdmittedEventBatch, EventBatchPlan};
use rw_types::{EngineEvent, EventMeta, SequenceId, SessionId};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify};

fn plan() -> EventBatchPlan {
    EventBatchPlan::new(vec![EngineEvent::TextDelta {
        meta: EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: SessionId("queue".to_owned()),
            sequence_id: SequenceId(0),
            emitted_at: "2026-09-05T00:00:00Z".to_owned(),
            caused_by: None,
        },
        turn_id: rw_types::TurnId("turn".to_owned()),
        text: "journal payload".to_owned(),
    }])
    .expect("plan")
}
fn admitted(queue: &JournalCommits) -> Arc<AdmittedEventBatch> {
    let plan = plan();
    let reservation = queue.reserve(&plan).expect("reserve");
    plan.prepare(reservation)
}
#[test]
fn queued_and_unconsumed_allocations_share_one_item_allowance() {
    let queue = JournalCommits::new();
    let held = (0..MAX_BATCHES)
        .map(|_| admitted(&queue))
        .collect::<Vec<_>>();
    assert!(queue.reserve(&plan()).is_err());
    drop(held);
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES);
    assert!(queue.reserve(&plan()).is_ok());
}
#[tokio::test]
async fn successful_reply_retains_admission_until_the_consumer_releases_it() {
    let queue = JournalCommits::new();
    let batch = admitted(&queue);
    let returned = Arc::clone(&batch);
    let order = Arc::new(Mutex::new(())).lock_owned().await;
    let reply = queue
        .execute(Arc::new(()), batch, order, async move { Ok(returned) })
        .await
        .expect("commit");
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES - 1);
    queue.shutdown().await.expect("actual work settled");
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES - 1);
    drop(reply);
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_waiter_keeps_native_work_owner_and_session_order_until_completion() {
    let queue = JournalCommits::new();
    let order = Arc::new(Mutex::new(()));
    let owner = Arc::new(String::from("owned session"));
    let weak = Arc::downgrade(&owner);
    let entered = Arc::new(Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    let caller = {
        let queue = Arc::clone(&queue);
        let entered = Arc::clone(&entered);
        let order = Arc::clone(&order);
        let batch = admitted(&queue);
        let returned = Arc::clone(&batch);
        tokio::spawn(async move {
            let guard = queue.enter(order).await.expect("session order");
            queue
                .execute(owner, batch, guard, async move {
                    tokio::task::spawn_blocking(move || {
                        entered.notify_one();
                        wait.recv_timeout(Duration::from_secs(5))
                            .expect("release native worker");
                    })
                    .await
                    .expect("native completion");
                    Ok(returned)
                })
                .await
        })
    };
    entered.notified().await;
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
    assert!(weak.upgrade().is_some());
    assert!(order.try_lock().is_err());
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES - 1);
    release.send(()).expect("release");
    queue.shutdown().await.expect("settled native work");
    assert!(weak.upgrade().is_none());
    assert!(order.try_lock().is_ok());
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES);
}
#[tokio::test]
async fn failed_commit_keeps_its_owner_and_rejects_later_order_waiters() {
    let queue = JournalCommits::new();
    let order = Arc::new(Mutex::new(()));
    let owner = Arc::new(String::from("retained session"));
    let weak = Arc::downgrade(&owner);
    let batch = admitted(&queue);
    let guard = queue.enter(Arc::clone(&order)).await.expect("order");
    assert!(
        queue
            .execute(owner, batch, guard, async {
                Err(failure("unproven write"))
            })
            .await
            .is_err()
    );
    assert!(queue.shutdown().await.is_err());
    assert!(weak.upgrade().is_some());
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES - 1);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), queue.enter(order))
            .await
            .expect("bounded rejection")
            .is_err()
    );
}
#[tokio::test]
async fn expired_proof_retains_actual_worker_until_it_finishes_and_stays_failed() {
    let mut queue = JournalCommits::new();
    Arc::get_mut(&mut queue)
        .expect("exclusive queue")
        .proof_timeout = Duration::from_millis(10);
    let order = Arc::new(Mutex::new(()));
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let batch = admitted(&queue);
    let returned = Arc::clone(&batch);
    let work = {
        let release = Arc::clone(&release);
        let completed = Arc::clone(&completed);
        async move {
            release.notified().await;
            completed.notify_one();
            Ok(returned)
        }
    };
    let guard = queue.enter(Arc::clone(&order)).await.expect("order");
    assert!(
        queue
            .execute(Arc::new(()), batch, guard, work)
            .await
            .is_err()
    );
    assert!(order.try_lock().is_err());
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES - 1);
    release.notify_one();
    completed.notified().await;
    assert!(queue.shutdown().await.is_err());
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES - 1);
}

#[test]
fn retained_string_capacity_exhausts_bytes_before_item_slots() {
    let queue = JournalCommits::new();
    let large_plan = || {
        let mut events = plan().events().to_vec();
        if let EngineEvent::TextDelta { text, .. } = &mut events[0] {
            let mut reserved = String::with_capacity(super::MAX_BYTES as usize / 2 + 1);
            reserved.push('x');
            *text = reserved;
        }
        EventBatchPlan::new(events).expect("capacity plan")
    };
    let first = large_plan();
    let reservation = queue.reserve(&first).expect("first capacity");
    let held = first.prepare(reservation);
    assert!(queue.reserve(&large_plan()).is_err());
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES - 1);
    drop(held);
    assert!(queue.reserve(&large_plan()).is_ok());
}

#[tokio::test]
async fn panicked_worker_returns_failed_proof_and_retains_its_session() {
    let queue = JournalCommits::new();
    let owner = Arc::new(String::from("panic owner"));
    let weak = Arc::downgrade(&owner);
    let batch = admitted(&queue);
    let order = Arc::new(Mutex::new(())).lock_owned().await;
    assert!(
        queue
            .execute(owner, batch, order, async {
                panic!("controlled commit worker panic")
            })
            .await
            .is_err()
    );
    assert!(queue.shutdown().await.is_err());
    assert!(weak.upgrade().is_some());
    assert_eq!(queue.batches.available_permits(), MAX_BATCHES - 1);
}
