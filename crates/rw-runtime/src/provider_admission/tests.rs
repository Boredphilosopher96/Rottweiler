#![allow(clippy::unwrap_used)]

use super::*;
use rw_store::session::{
    UtcTimestamp,
    reservations::{BudgetCharge, BudgetChargeBound, ProviderCallPhase},
};
use rw_types::{AccountingAttribution, SessionId, TurnId, config::BudgetConfig};

fn plan() -> BudgetReservationPlan {
    BudgetReservationPlan {
        identity: ProviderCallIdentity {
            budget_session_id: SessionId("session".into()),
            session_id: SessionId("session".into()),
            turn_id: TurnId("turn".into()),
            attribution: AccountingAttribution::Main,
            call_id: "call".into(),
            attempt: 0,
        },
        admitted_at: UtcTimestamp::parse("2026-09-04T12:00:00.000Z").unwrap(),
        input_token_bound: 100,
        output_token_limit: 100,
        charge: BudgetChargeBound::Bounded(BudgetCharge::UsdMicros(80)),
        budget: BudgetConfig {
            session_cost_cap_micros_usd: Some(100),
            ..BudgetConfig::default()
        },
    }
}

#[tokio::test]
async fn completed_unconsumed_replies_keep_admission_credit() {
    let root = tempfile::tempdir().unwrap();
    let service = DurableProviderAdmission::open(root.path().to_owned())
        .await
        .unwrap();
    let mut replies = Vec::new();
    let mut completions = Vec::new();
    for _ in 0..MAX_STORAGE_JOBS {
        let (done, completion) = oneshot::channel();
        replies.push(
            service
                .enqueue(move |_| {
                    let _ = done.send(());
                    Ok(())
                })
                .unwrap(),
        );
        completions.push(completion);
    }
    for completion in completions {
        completion.await.unwrap();
    }
    assert!(matches!(service.enqueue(|_| Ok(())), Err(Error::Capacity)));
    drop(replies.pop());
    service.request(|_| Ok(())).await.unwrap();
    drop(replies);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn caller_loss_does_not_cancel_an_admitted_commit() {
    let root = tempfile::tempdir().unwrap();
    let service = DurableProviderAdmission::open(root.path().to_owned())
        .await
        .unwrap();
    let (entered, entry) = oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let blocker = service
        .enqueue(move |_| {
            let _ = entered.send(());
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
    entry.await.unwrap();
    let request = plan();
    let identity = request.identity.clone();
    let reply = service
        .enqueue(move |ledger| ledger.reserve(&request))
        .unwrap();
    drop(reply);
    release.send(()).unwrap();
    blocker.await.unwrap().result.unwrap();
    assert_eq!(
        service
            .request(move |ledger| ledger.phase(&identity))
            .await
            .unwrap(),
        Some(ProviderCallPhase::Reserved)
    );
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_started_permit_preserves_the_durable_charge() {
    let root = tempfile::tempdir().unwrap();
    let service = DurableProviderAdmission::open(root.path().to_owned())
        .await
        .unwrap();
    let active = service
        .reserve(plan())
        .await
        .unwrap()
        .start()
        .await
        .unwrap();
    drop(active);
    let mut next = plan();
    next.identity.call_id = "next".into();
    next.charge = BudgetChargeBound::Bounded(BudgetCharge::UsdMicros(21));
    assert!(matches!(
        service.reserve(next).await,
        Err(Error::CapExceeded { reserved: 80, .. })
    ));
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_shutdown_wait_still_settles_prior_work() {
    let root = tempfile::tempdir().unwrap();
    let service = DurableProviderAdmission::open(root.path().to_owned())
        .await
        .unwrap();
    let (entered, entry) = oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let reply = service
        .enqueue(move |_| {
            let _ = entered.send(());
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
    entry.await.unwrap();
    let copy = service.clone();
    let shutdown = tokio::spawn(async move { copy.shutdown().await });
    while !service.worker.closed.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
    shutdown.abort();
    assert!(matches!(
        service.reserve(plan()).await,
        Err(Error::Worker(_))
    ));
    assert!(service.worker.finished.borrow().is_none());
    release.send(()).unwrap();
    reply.await.unwrap().result.unwrap();
    service.shutdown().await.unwrap();
    assert_eq!(*service.worker.finished.borrow(), Some(true));
}

#[test]
fn idle_accounting_owner_does_not_occupy_tokio_blocking_capacity() {
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let service = DurableProviderAdmission::open(root.path().to_owned())
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::task::spawn_blocking(|| 7),
        )
        .await
        .unwrap()
        .unwrap();
        service.shutdown().await.unwrap();
    });
}
