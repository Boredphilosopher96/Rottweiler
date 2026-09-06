#![allow(clippy::expect_used)]
use super::*;
use tokio::sync::oneshot;

#[tokio::test]
async fn saturated_preparation_does_not_occupy_global_workers() {
    let spawner = Arc::new(DeferredLspSpawner::new(&[]));
    let held = spawner.admission().await.expect("first preparation");
    let mut queued = tokio::task::JoinSet::new();
    for _ in 0..MAX_PREPARATION_WAITERS {
        let spawner = Arc::clone(&spawner);
        queued.spawn(async move { spawner.admission().await.map(drop) });
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while spawner.waiting.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded queue populated");
    assert!(spawner.admission().await.is_err());
    tokio::time::timeout(
        Duration::from_secs(2),
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, || 42),
    )
    .await
    .expect("unrelated work is not queued behind LSP preparation")
    .expect("worker");
    queued.abort_all();
    while queued.join_next().await.is_some() {}
    assert_eq!(spawner.waiting.available_permits(), MAX_PREPARATION_WAITERS);
    assert!(spawner.prepared.try_lock().is_err());
    drop(held);
    assert!(spawner.admission().await.is_ok());
}

#[tokio::test]
async fn cancelled_preparation_caller_cannot_release_the_physical_slot() {
    let root = tempfile::tempdir().expect("workspace");
    let spawner = Arc::new(DeferredLspSpawner::new(&[root.path().to_path_buf()]));
    let mut slot = spawner.admission().await.expect("preparation owner");
    let roots = Arc::clone(&spawner.roots);
    let (entered, started) = oneshot::channel();
    let (release, finish) = std::sync::mpsc::sync_channel(1);
    let (published, publication) = oneshot::channel();
    let caller = tokio::spawn(async move {
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            entered.send(()).expect("entered worker");
            finish.recv().expect("preparation release");
            let owner = prepare(&roots, &mut slot).expect("prepare once");
            let _ = published.send(Arc::downgrade(&owner));
        })
        .await
    });
    started.await.expect("physical worker");
    caller.abort();
    let _ = caller.await;
    assert!(spawner.prepared.try_lock().is_err());
    let mut next = Box::pin(spawner.admission());
    tokio::select! {
        biased;
        _ = &mut next => panic!("cancelled caller released physical preparation"),
        () = tokio::task::yield_now() => {}
    }
    release.send(()).expect("settle preparation");
    let first = publication.await.expect("publication despite caller loss");
    let mut next = next.await.expect("next preparation owner");
    let same = prepare(&spawner.roots, &mut next).expect("reuse prepared authority");
    assert!(Arc::ptr_eq(
        &first.upgrade().expect("retained authority"),
        &same
    ));
}

#[tokio::test]
async fn discarded_worker_result_retires_the_child_before_releasing_scratch() {
    let root = tempfile::tempdir().expect("workspace");
    let spawner = DeferredLspSpawner::new(&[root.path().to_path_buf()]);
    let mut slot = spawner.admission().await.expect("launch owner");
    let roots = Arc::clone(&spawner.roots);
    let workspace = root.path().to_path_buf();
    let server = LspServerConfig {
        language: rw_tools::Language::Rust,
        command: PathBuf::from("/bin/cat"),
        args: Vec::new(),
    };
    let runtime = tokio::runtime::Handle::current();
    let (entered, launched) = oneshot::channel();
    let (release, finish) = std::sync::mpsc::sync_channel(1);
    let caller = tokio::spawn(async move {
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let child =
                launch(&roots, &mut slot, &workspace, &server, &runtime).expect("physical child");
            let owner = slot.as_ref().expect("prepared owner");
            entered
                .send((Arc::downgrade(owner), owner.scratch.path().to_path_buf()))
                .expect("child ready");
            finish.recv().expect("result transfer release");
            child
        })
        .await
    });
    let (owner, scratch) = launched.await.expect("launched");
    caller.abort();
    let _ = caller.await;
    drop(spawner);
    assert!(scratch.is_dir());
    assert!(owner.upgrade().is_some());
    release.send(()).expect("discard child result");
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child proof retires launch authority");
    assert!(!scratch.exists());
}
