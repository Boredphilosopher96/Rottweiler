#![allow(clippy::expect_used)]
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Unsettled {
    signals: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
}
#[async_trait]
impl LspProcessHandle for Unsettled {
    fn request_termination(&mut self) -> io::Result<()> {
        self.signals.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn kill(&mut self) -> io::Result<()> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

struct Fixture {
    handle: OwnedLspHandle,
    owner: Arc<Prepared>,
    signals: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
}
fn retirement_fixture() -> Fixture {
    let root = tempfile::tempdir().expect("workspace");
    let owner = prepare(&[root.path().to_path_buf()], &mut None).expect("authority");
    let signals = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let handle = OwnedLspHandle(Some(PhysicalLsp {
        handle: Box::new(Unsettled {
            signals: Arc::clone(&signals),
            polls: Arc::clone(&polls),
        }),
        owner: Arc::clone(&owner),
    }));
    Fixture {
        handle,
        owner,
        signals,
        polls,
    }
}

#[test]
fn dropping_without_a_runtime_requests_termination_before_quarantine() {
    let Fixture {
        handle,
        owner,
        signals,
        polls,
    } = retirement_fixture();
    drop(handle);
    assert_eq!(signals.load(Ordering::SeqCst), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&owner), 2, "unproven authority retained");
    assert!(owner.scratch.path().is_dir());
    // This fake has no physical effect. Remove its intentionally quarantined
    // scratch fixture; production must retain scratch without settlement proof.
    std::fs::remove_dir_all(owner.scratch.path()).expect("fixture cleanup");
}

#[test]
fn runtime_discarding_unpolled_cleanup_has_already_requested_termination() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let Fixture {
        handle,
        owner,
        signals,
        polls,
    } = retirement_fixture();
    runtime.block_on(async {
        drop(handle);
        assert_eq!(signals.load(Ordering::SeqCst), 1);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    });
    drop(runtime);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(
        Arc::strong_count(&owner),
        2,
        "unpolled proof is quarantined"
    );
    assert!(owner.scratch.path().is_dir());
    std::fs::remove_dir_all(owner.scratch.path()).expect("fake fixture cleanup");
}
