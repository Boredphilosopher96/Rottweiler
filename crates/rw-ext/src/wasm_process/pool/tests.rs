#![allow(clippy::expect_used)]
use super::*;
use std::os::unix::fs::PermissionsExt as _;

fn generation(helper: PathBuf) -> Arc<Generation> {
    Arc::new(Generation {
        helper,
        manifest: PluginManifest {
            name: "pool-test".to_owned(),
            version: "1.0.0".to_owned(),
            protocol: rw_plugin_protocol::PROTOCOL_VERSION,
            capabilities: rw_plugin_protocol::PluginCapabilities::default(),
        },
        component: Arc::from(&b"component"[..]),
        limits: WasmHookLimits::default(),
        digest: blake3::hash(b"test"),
        jobs: Mutex::default(),
    })
}

#[tokio::test]
async fn saturation_is_bounded_before_starting_or_copying_components() {
    let pool = WasmWorkerPool::capacity(1);
    let held = pool
        .execution
        .acquire()
        .await
        .expect("hold worker admission");
    let generation = generation(PathBuf::from("unused-helper"));
    let mut tasks = Vec::new();
    for _ in 0..MAX_ADMITTED {
        let pool = pool.clone();
        let generation = generation.clone();
        tasks.push(tokio::spawn(async move {
            pool.request(generation, None, Duration::from_secs(5)).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while pool.admission.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all slots admitted");
    assert!(
        pool.request(generation.clone(), None, Duration::from_secs(5))
            .await
            .is_err()
    );
    assert_eq!(pool.stats().process_starts, 0);
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
    generation.settle().await.expect("settled");
    assert_eq!(pool.admission.available_permits(), MAX_ADMITTED);
    assert!(generation.jobs.lock().expect("jobs").is_empty());
    drop(held);
    pool.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn dropped_caller_reaps_worker_before_settlement_and_releases_its_slot() {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("helper");
    let pid_path = root.path().join("pid");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nwhile :; do :; done\n",
            pid_path.display()
        ),
    )
    .expect("helper");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("executable");
    let pool = WasmWorkerPool::capacity(1);
    let generation = generation(path);
    let call = {
        let pool = pool.clone();
        let generation = generation.clone();
        tokio::spawn(async move { pool.request(generation, None, Duration::from_secs(5)).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !pid_path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("helper started");
    call.abort();
    let _ = call.await;
    tokio::time::timeout(Duration::from_secs(2), generation.settle())
        .await
        .expect("owned cleanup")
        .expect("settled");
    let pid = std::fs::read_to_string(pid_path).expect("pid");
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("probe");
    assert!(!alive.success(), "settlement requires a reaped child");
    assert_eq!(pool.execution.available_permits(), 1);
    assert_eq!(pool.admission.available_permits(), MAX_ADMITTED);
    assert!(generation.jobs.lock().expect("jobs").is_empty());
    let timeout = pool
        .request(generation.clone(), None, Duration::from_millis(100))
        .await
        .expect_err("fixed deadline");
    assert!(timeout.to_string().contains("deadline"));
    let pid = std::fs::read_to_string(root.path().join("pid")).expect("replacement pid");
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("probe replacement");
    assert!(!alive.success(), "timeout also waits for reap");
    assert_eq!(pool.execution.available_permits(), 1);
    pool.shutdown().await.expect("shutdown");
}

async fn admitted_owner(pool: &Arc<WasmWorkerPool>, generation: &Arc<Generation>) -> JobOwner {
    let job = Arc::new(JobState::new());
    pool.jobs.lock().expect("pool jobs").push(Arc::clone(&job));
    generation
        .jobs
        .lock()
        .expect("generation jobs")
        .push(Arc::clone(&job));
    let admission = Arc::clone(&pool.admission)
        .acquire_owned()
        .await
        .expect("admission");
    let mut owner = JobOwner::new(Arc::clone(pool), Arc::clone(generation), job, admission);
    owner.execution = Some(
        Arc::clone(&pool.execution)
            .acquire_owned()
            .await
            .expect("execution"),
    );
    owner.worker = Some(Worker::start(std::path::Path::new("/bin/sh")).expect("child"));
    owner
}

#[tokio::test]
async fn failed_reap_returns_error_and_never_releases_failed_capacity() {
    let pool = WasmWorkerPool::capacity(1);
    let generation = generation(PathBuf::from("/bin/sh"));
    let mut owner = admitted_owner(&pool, &generation).await;
    // Consume the actual OS wait result outside Tokio, creating a real ECHILD
    // failure without leaving a live child behind the test.
    let worker = owner.worker.as_mut().expect("worker");
    let pid = rustix::process::Pid::from_raw(
        i32::try_from(worker.child.id().expect("pid")).expect("pid range"),
    )
    .expect("nonzero pid");
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("kill fixture");
    rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty())
        .expect("external reap")
        .expect("reaped");
    let error = tokio::time::timeout(Duration::from_secs(2), owner.retire_worker())
        .await
        .expect("bounded failed proof")
        .expect_err("Tokio no longer owns wait result");
    assert!(error.to_string().contains("reap failed"));
    owner.finish();
    drop(owner);
    assert!(generation.settle().await.is_err());
    assert_eq!(pool.admission.available_permits(), MAX_ADMITTED - 1);
    assert_eq!(pool.execution.available_permits(), 0);
    assert_eq!(pool.quarantined.lock().expect("quarantine").len(), 1);
    assert!(
        pool.request(generation, None, Duration::from_secs(1))
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), pool.shutdown())
            .await
            .expect("shutdown reports failure")
            .is_err()
    );
    assert_eq!(
        pool.quarantined
            .lock()
            .expect("failed worker retained")
            .len(),
        1
    );
}

#[tokio::test]
async fn aborted_owner_reports_failed_proof_and_shutdown_reaps_quarantined_child() {
    let pool = WasmWorkerPool::capacity(1);
    let generation = generation(PathBuf::from("/bin/sh"));
    let owner = admitted_owner(&pool, &generation).await;
    let pid = owner
        .worker
        .as_ref()
        .expect("worker")
        .child
        .id()
        .expect("pid");
    let task = tokio::spawn(async move {
        let _owner = owner;
        std::future::pending::<()>().await;
    });
    task.abort();
    assert!(task.await.expect_err("aborted").is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), generation.settle())
            .await
            .expect("explicit failed proof")
            .is_err()
    );
    assert_eq!(pool.admission.available_permits(), MAX_ADMITTED - 1);
    assert_eq!(pool.execution.available_permits(), 0);
    assert_eq!(pool.quarantined.lock().expect("owned child").len(), 1);
    assert!(pool.shutdown().await.is_err(), "lost proof remains sticky");
    assert!(
        pool.quarantined
            .lock()
            .expect("shutdown actually reaped child")
            .is_empty()
    );
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("probe");
    assert!(
        !alive.success(),
        "shutdown settled the retained native child"
    );
    assert_eq!(pool.admission.available_permits(), MAX_ADMITTED - 1);
}

#[tokio::test]
async fn retirement_deadline_keeps_native_ownership_and_capacity_charged() {
    let pool = WasmWorkerPool::capacity(1);
    let generation = generation(PathBuf::from("/bin/sh"));
    let mut owner = admitted_owner(&pool, &generation).await;
    // A controlled wait proves timer behavior without relying on an OS process
    // entering an uninterruptible state. The owner retains a real child.
    let result = reap_before(std::future::pending(), tokio::time::Instant::now()).await;
    assert!(
        result
            .expect_err("proof deadline")
            .to_string()
            .contains("deadline expired")
    );
    owner.finish();
    drop(owner);
    assert!(generation.settle().await.is_err());
    assert_eq!(pool.admission.available_permits(), MAX_ADMITTED - 1);
    assert_eq!(pool.execution.available_permits(), 0);
    assert_eq!(pool.quarantined.lock().expect("retained child").len(), 1);
    assert!(pool.shutdown().await.is_err());
    assert!(pool.quarantined.lock().expect("actual cleanup").is_empty());
}
