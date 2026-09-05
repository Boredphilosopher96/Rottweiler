#![allow(clippy::expect_used)]

use super::{MAX_FIXTURE_BYTES, PROOF_TIMEOUT, ReplayReads, read_fixture};
use crate::ProviderErrorKind;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

#[tokio::test]
async fn cancelled_reader_retains_actual_worker_and_admission_until_completion() {
    let owner = Arc::new(ReplayReads::default());
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let read_owner = Arc::clone(&owner);
    let reader = tokio::spawn(async move {
        read_owner
            .run(move || {
                entered.send(()).expect("reader started");
                wait.recv().expect("release worker");
                Ok(vec![1, 2, 3])
            })
            .await
    });
    started.await.expect("worker entry");
    reader.abort();
    assert!(reader.await.expect_err("reader cancelled").is_cancelled());
    assert!(owner.begin().is_err(), "abandoned worker keeps admission");
    let settlement = owner.settle();
    tokio::pin!(settlement);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut settlement)
            .await
            .is_err()
    );
    release.send(()).expect("worker release");
    settlement.await.expect("actual worker settled");
    assert!(owner.current.lock().expect("slot").is_none());
    assert_eq!(owner.run(|| Ok(vec![4])).await.expect("next read"), vec![4]);
}

#[tokio::test(start_paused = true)]
async fn proof_timeout_remains_failed_and_retains_job_after_late_completion() {
    let owner = Arc::new(ReplayReads::default());
    let mut lease = owner.begin().expect("read admission");
    lease.started = true;
    let job = Arc::clone(&lease.job);
    drop(lease);
    let proof_owner = Arc::clone(&owner);
    let proof = tokio::spawn(async move { proof_owner.settle().await });
    tokio::task::yield_now().await;
    tokio::time::advance(PROOF_TIMEOUT).await;
    assert_eq!(
        proof.await.expect("proof task").expect_err("deadline").kind,
        ProviderErrorKind::EffectsUnsettled
    );
    job.done.store(true, Ordering::Release);
    assert_eq!(
        owner.settle().await.expect_err("sticky failure").kind,
        ProviderErrorKind::EffectsUnsettled
    );
    assert!(owner.begin().is_err());
}

#[tokio::test]
async fn completed_unconsumed_result_keeps_admission_and_worker_panic_unwinds_before_proof() {
    let owner = Arc::new(ReplayReads::default());
    let lease = owner.begin().expect("read admission");
    *lease.job.result.lock().expect("result") = Some(Ok(vec![1]));
    lease.job.done.store(true, Ordering::Release);
    assert!(owner.begin().is_err());
    assert_eq!(lease.finish().await.expect("consume result"), vec![1]);
    assert_eq!(
        owner
            .run(|| panic!("injected read panic"))
            .await
            .expect_err("panic outcome")
            .kind,
        ProviderErrorKind::Protocol
    );
    owner.settle().await.expect("read frame unwound");
    assert!(owner.current.lock().expect("slot").is_none());
}

#[test]
fn oversized_sparse_fixture_is_rejected_before_payload_allocation() {
    let path = std::env::temp_dir().join(format!("rw-replay-read-limit-{}", std::process::id()));
    let file = std::fs::File::create(&path).expect("fixture");
    file.set_len((MAX_FIXTURE_BYTES + 1) as u64)
        .expect("sparse fixture");
    drop(file);
    let result = read_fixture(path.clone());
    std::fs::remove_file(path).expect("cleanup");
    assert_eq!(
        result.expect_err("encoded admission").kind,
        ProviderErrorKind::Protocol
    );
}

#[tokio::test]
async fn rejected_request_releases_unstarted_read_without_waiting_for_a_worker() {
    let owner = Arc::new(ReplayReads::default());
    drop(owner.begin().expect("request admission"));
    owner.settle().await.expect("no local effect started");
    assert!(owner.current.lock().expect("slot").is_none());
}
