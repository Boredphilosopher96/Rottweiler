#![allow(clippy::expect_used)]
use super::*;
use std::{
    sync::Weak,
    time::{Duration, Instant},
};

struct Fixture {
    owner: NativeCommandOwner,
    cleanup: Arc<NativeCleanup>,
    scratch: Weak<CommandScratch>,
    path: PathBuf,
}
fn fixture() -> Fixture {
    let scratch = CommandScratch::create("retirement-fixture").expect("scratch");
    let path = scratch.path().to_path_buf();
    let retained = Arc::downgrade(&scratch);
    let cleanup = Arc::new(NativeCleanup::default());
    let child = Command::new("/bin/sh")
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .expect("child");
    let state = NativeCommandState {
        _scratch: Some(scratch),
        _helper: None,
        _process_credit: rw_resources::try_acquire(rw_resources::ResourceClass::Process)
            .expect("process credit"),
        _admission: Arc::clone(&cleanup.admission)
            .try_acquire_owned()
            .expect("admission"),
        child_id: child.id(),
        child,
        watchdog: None,
        output: None,
        proxy: None,
        proxy_cleanup: None,
        proxy_failure: None,
        _lease: None,
    };
    Fixture {
        owner: NativeCommandOwner {
            state: Some(state),
            cleanup: Arc::clone(&cleanup),
        },
        cleanup,
        scratch: retained,
        path,
    }
}
fn pending(cleanup: &NativeCleanup) -> Arc<NativeJob> {
    let jobs = cleanup.pending.lock().expect("retirements");
    assert_eq!(jobs.len(), 1);
    Arc::clone(&jobs[0])
}
fn assert_synchronously_stopped(cleanup: &NativeCleanup) {
    let job = pending(cleanup);
    let mut slot = job.state.try_lock().expect("proof task was not polled");
    let state = slot.as_mut().expect("physical state retained");
    let deadline = Instant::now() + Duration::from_secs(2);
    while state
        .child
        .try_wait()
        .expect("reap signal recipient")
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "signal requires a polled cleanup task"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn release_fixture(cleanup: &NativeCleanup) {
    // Quarantine intentionally retains real ownership. Once synchronous exit
    // has been observed, obtain the remaining group proof for this fixture.
    let job = pending(cleanup);
    let mut state = job
        .state
        .try_lock()
        .expect("retained state")
        .take()
        .expect("state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("cleanup runtime");
    runtime
        .block_on(state.settle())
        .expect("physical fixture proof");
    drop(state);
}

#[test]
fn absent_runtime_signals_and_retains_scratch_until_physical_proof() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let Fixture {
        owner,
        cleanup,
        scratch,
        path,
    } = {
        let _runtime = runtime.enter();
        fixture()
    };
    drop(owner);
    assert_synchronously_stopped(&cleanup);
    assert!(scratch.upgrade().is_some());
    assert!(path.is_dir());
    assert!(matches!(
        *pending(&cleanup).completion.borrow(),
        Some(Err(_))
    ));
    drop(runtime);
    release_fixture(&cleanup);
    assert!(scratch.upgrade().is_none());
    assert!(!path.exists());
}

#[test]
fn discarded_unpolled_retirement_signals_and_quarantines_scratch() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (cleanup, scratch, path) = runtime.block_on(async {
        let Fixture {
            owner,
            cleanup,
            scratch,
            path,
        } = fixture();
        drop(owner);
        assert_synchronously_stopped(&cleanup);
        assert!(pending(&cleanup).completion.borrow().is_none());
        (cleanup, scratch, path)
    });
    drop(runtime);
    assert!(scratch.upgrade().is_some());
    assert!(path.is_dir());
    assert!(matches!(
        *pending(&cleanup).completion.borrow(),
        Some(Err(_))
    ));
    release_fixture(&cleanup);
    assert!(scratch.upgrade().is_none());
    assert!(!path.exists());
}
