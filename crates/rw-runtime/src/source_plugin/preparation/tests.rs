#![allow(clippy::expect_used)]
use super::*;
use async_trait::async_trait;
use rw_ext::{
    CapabilityViolation, LaunchedPluginProcess, PluginProcessError, SupervisedPluginProcess,
};
use std::{path::PathBuf, sync::atomic::AtomicUsize, time::Duration};
use tokio::io::BufReader;

#[derive(Default)]
struct Launcher {
    launches: AtomicUsize,
    children: Mutex<Vec<Arc<Child>>>,
}
struct Child {
    inner: Mutex<tokio::process::Child>,
    group: Option<u32>,
    settled: AtomicBool,
}
#[async_trait]
impl SupervisedPluginProcess for Child {
    fn mark_capability_violation(&self, _: &CapabilityViolation) {}
    fn kill_tree(&self) -> std::result::Result<(), PluginProcessError> {
        if let Some(pid) = self
            .group
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        self.inner
            .lock()
            .expect("child lock")
            .start_kill()
            .map_err(process_error)
    }
    async fn wait(&self) -> std::result::Result<Option<i32>, PluginProcessError> {
        loop {
            if let Some(status) = self
                .inner
                .lock()
                .expect("child lock")
                .try_wait()
                .map_err(process_error)?
            {
                return Ok(status.code());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    async fn settle_effects(&self) -> std::result::Result<(), PluginProcessError> {
        self.wait().await?;
        rw_tools::terminate_and_wait_process_group(self.group)
            .await
            .map_err(process_error)?;
        self.settled.store(true, Ordering::Release);
        Ok(())
    }
}
fn process_error(error: impl std::fmt::Display) -> PluginProcessError {
    PluginProcessError {
        message: error.to_string(),
    }
}
#[async_trait]
impl PluginLauncher for Launcher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        _: &PluginSandboxProfile,
    ) -> std::result::Result<LaunchedPluginProcess, PluginProcessError> {
        use std::os::unix::process::CommandExt as _;
        let mut command = tokio::process::Command::new(config.executable());
        command
            .args(config.argv())
            .current_dir(config.cwd())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().map_err(process_error)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let child = Arc::new(Child {
            group: child.id(),
            inner: Mutex::new(child),
            settled: AtomicBool::new(false),
        });
        self.children
            .lock()
            .expect("children lock")
            .push(Arc::clone(&child));
        self.launches.fetch_add(1, Ordering::Release);
        Ok(LaunchedPluginProcess {
            stdin: Box::pin(stdin),
            stdout: Box::pin(BufReader::new(stdout)),
            stderr: Box::pin(BufReader::new(stderr)),
            process: child,
            executable_identity: config.executable_identity().clone(),
        })
    }
}
fn request(launcher: &Arc<Launcher>, script: &str) -> PreparationRequest {
    let scratch = Arc::new(PrivateMcpScratch::create().expect("private scratch"));
    let code = scratch.path().join("code");
    std::fs::create_dir(&code).expect("private code");
    let config = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
        .and_then(|config| config.with_argv(["-c", script]))
        .and_then(|config| config.with_cwd(&code))
        .expect("helper config");
    PreparationRequest {
        config,
        output_root: None,
        launcher: launcher.clone(),
        scratch,
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("fixture condition");
}
fn assert_idle(pool: &SourcePreparations, launcher: &Launcher) {
    assert!(pool.jobs.lock().expect("jobs lock").is_empty());
    assert_eq!(pool.budget.admission.available_permits(), MAX_PREPARATIONS);
    assert_eq!(
        pool.budget.execution.available_permits(),
        CONCURRENT_PREPARATIONS
    );
    assert!(
        launcher
            .children
            .lock()
            .expect("children lock")
            .iter()
            .all(|child| child.settled.load(Ordering::Acquire))
    );
}
#[tokio::test]
async fn dropping_preparation_retains_scratch_and_reaps_owned_children() {
    let pool = Arc::new(SourcePreparations::default());
    let launcher = Arc::new(Launcher::default());
    let request = request(&launcher, "sleep 60 & wait");
    let scratch = request.scratch.path().to_owned();
    let executing = Arc::clone(&pool);
    let caller = tokio::spawn(async move {
        executing
            .execute(request, Instant::now() + Duration::from_secs(30))
            .await
    });
    until(|| launcher.launches.load(Ordering::Acquire) == 1).await;
    assert!(scratch.exists());
    caller.abort();
    assert!(caller.await.expect_err("caller dropped").is_cancelled());
    tokio::time::timeout(Duration::from_secs(3), pool.settle_cancelled())
        .await
        .expect("owned cleanup completes");
    assert_idle(&pool, &launcher);
    assert!(!scratch.exists());
}
#[tokio::test]
async fn deadline_returns_only_after_native_effects_and_pipes_settle() {
    let pool = Arc::new(SourcePreparations::default());
    let launcher = Arc::new(Launcher::default());
    let result = pool
        .execute(
            request(&launcher, "sleep 60 & wait"),
            Instant::now() + Duration::from_millis(100),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(launcher.launches.load(Ordering::Acquire), 1);
    assert_idle(&pool, &launcher);
}
#[tokio::test]
async fn queued_admission_is_bounded_and_abandoned_entries_self_retire() {
    let pool = Arc::new(SourcePreparations::default());
    let launcher = Arc::new(Launcher::default());
    let mut callers = Vec::new();
    for _ in 0..MAX_PREPARATIONS {
        let executing = Arc::clone(&pool);
        let request = request(&launcher, "sleep 60 & wait");
        callers.push(tokio::spawn(async move {
            executing
                .execute(request, Instant::now() + Duration::from_secs(30))
                .await
        }));
    }
    until(|| pool.jobs.lock().expect("jobs lock").len() == MAX_PREPARATIONS).await;
    assert!(launcher.launches.load(Ordering::Acquire) <= CONCURRENT_PREPARATIONS);
    assert!(
        pool.execute(
            request(&launcher, "exit 0"),
            Instant::now() + Duration::from_secs(30)
        )
        .await
        .is_err()
    );
    for caller in &callers {
        caller.abort();
    }
    for caller in callers {
        assert!(caller.await.expect_err("caller dropped").is_cancelled());
    }
    tokio::time::timeout(Duration::from_secs(3), pool.settle_cancelled())
        .await
        .expect("all owned cleanup completes");
    assert_idle(&pool, &launcher);
    assert!(launcher.launches.load(Ordering::Acquire) <= CONCURRENT_PREPARATIONS);
}
#[tokio::test]
async fn success_preserves_both_streams_and_retires_without_a_followup_call() {
    let pool = Arc::new(SourcePreparations::default());
    let launcher = Arc::new(Launcher::default());
    let result = pool
        .execute(
            request(&launcher, "printf output; printf diagnostic >&2"),
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .expect("helper completed");
    assert_eq!(result.stdout, b"output");
    assert_eq!(result.stderr, b"diagnostic");
    assert_eq!(result.status, Some(0));
    assert_idle(&pool, &launcher);
}

#[tokio::test]
async fn output_overflow_cancels_other_pipe_reads_without_waiting_for_deadline() {
    let pool = Arc::new(SourcePreparations::default());
    let launcher = Arc::new(Launcher::default());
    let script = format!("head -c {} /dev/zero; sleep 60", MAX_REPORT_BYTES + 1);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        pool.execute(
            request(&launcher, &script),
            Instant::now() + Duration::from_secs(30),
        ),
    )
    .await
    .expect("overflow fails promptly");
    assert!(result.is_err());
    assert_idle(&pool, &launcher);
}

struct PanickingLauncher;
#[async_trait]
impl PluginLauncher for PanickingLauncher {
    async fn launch(
        &self,
        _: &PluginProcessConfig,
        _: &PluginSandboxProfile,
    ) -> std::result::Result<LaunchedPluginProcess, PluginProcessError> {
        panic!("seeded panic after preparation admission");
    }
}
#[tokio::test]
async fn executor_panic_cannot_release_admission_or_pass_the_settlement_barrier() {
    let pool = Arc::new(SourcePreparations::default());
    let mut operation_request = request(&Arc::new(Launcher::default()), "exit 0");
    operation_request.launcher = Arc::new(PanickingLauncher);
    let scratch = operation_request.scratch.path().to_owned();
    let executing = Arc::clone(&pool);
    let caller = tokio::spawn(async move {
        executing
            .execute(operation_request, Instant::now() + Duration::from_secs(3))
            .await
    });
    until(|| pool.budget.execution.available_permits() == CONCURRENT_PREPARATIONS - 1).await;
    caller.abort();
    assert!(caller.await.expect_err("caller dropped").is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), pool.settle_cancelled())
            .await
            .is_err()
    );
    assert_eq!(
        pool.budget.admission.available_permits(),
        MAX_PREPARATIONS - 1
    );
    assert_eq!(
        pool.budget.execution.available_permits(),
        CONCURRENT_PREPARATIONS - 1
    );
    assert_eq!(pool.jobs.lock().expect("jobs lock").len(), 1);
    assert!(
        scratch.exists(),
        "unproven work retains its private scratch"
    );
    let other = Arc::new(SourcePreparations::new(Arc::clone(&pool.budget)));
    tokio::time::timeout(Duration::from_secs(1), other.settle_cancelled())
        .await
        .expect("unrelated generation does not inherit an unproven barrier");
    let launcher = Arc::new(Launcher::default());
    let result = other
        .execute(
            request(&launcher, "printf healthy"),
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .expect("spare shared capacity remains usable");
    assert_eq!(result.stdout, b"healthy");
    // This fixture panics before spawning native work; remove its retained directory.
    std::fs::remove_dir_all(&scratch).expect("remove inert panic fixture scratch");
    assert_eq!(
        pool.budget.admission.available_permits(),
        MAX_PREPARATIONS - 1
    );
}

#[tokio::test]
async fn undelivered_completion_keeps_its_admission_charge() {
    let pool = Arc::new(SourcePreparations::default());
    let launcher = Arc::new(Launcher::default());
    let output = pool.execute(
        request(&launcher, "printf completed"),
        Instant::now() + Duration::from_secs(3),
    );
    tokio::pin!(output);
    assert!(futures_util::poll!(output.as_mut()).is_pending());
    until(|| {
        launcher.launches.load(Ordering::Acquire) == 1
            && pool.jobs.lock().expect("jobs lock").is_empty()
    })
    .await;
    assert_eq!(
        pool.budget.admission.available_permits(),
        MAX_PREPARATIONS - 1
    );
    assert_eq!(
        pool.budget.execution.available_permits(),
        CONCURRENT_PREPARATIONS
    );
    assert_eq!(output.await.expect("owned result").stdout, b"completed");
    assert_idle(&pool, &launcher);
}
