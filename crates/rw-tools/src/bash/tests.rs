#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rw_sandbox::SandboxPolicy;
use rw_types::{ToolCapability, ToolOutputStream};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::Duration;

use crate::registry::{
    CancellationToken, Tool, ToolContext, ToolError, ToolLimits, ToolOutputChunk, ToolOutputSink,
};

use tempfile::tempdir;

use super::*;

use super::execution_lease::*;
use super::native::*;
use super::output::*;
use super::process_group::*;
use super::replay::*;
use super::safety::*;

struct StreamingExecutor;

struct SecretRedactor;

impl CommandFixtureRedactor for SecretRedactor {
    fn redact(&self, value: &str) -> String {
        value.replace("secret-canary", "[REDACTED]")
    }
}

#[async_trait]
impl CommandExecutor for StreamingExecutor {
    async fn settle_effects(&self) {}
    async fn run(
        &self,
        _request: CommandRequest,
        _cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        output
            .emit(ToolOutputChunk {
                stream: ToolOutputStream::Stdout,
                content: "0123456789".to_owned(),
            })
            .await?;
        output
            .emit(ToolOutputChunk {
                stream: ToolOutputStream::Stderr,
                content: "warning".to_owned(),
            })
            .await?;
        Ok(CommandOutcome { exit_code: 7 })
    }
}

#[derive(Default)]
struct RecordingSink(Mutex<Vec<ToolOutputChunk>>);

#[async_trait]
impl ToolOutputSink for RecordingSink {
    async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
        self.0
            .lock()
            .map_err(|_| ToolError::Output("test lock".to_owned()))?
            .push(chunk);
        Ok(())
    }
}

struct PanickingExecutor;

#[async_trait]
impl CommandExecutor for PanickingExecutor {
    async fn settle_effects(&self) {}
    async fn run(
        &self,
        _request: CommandRequest,
        _cancellation: CancellationToken,
        _output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        panic!("injected executor panic");
    }
}

#[derive(Default)]
struct PanicDuringNative {
    executor: TokioCommandExecutor,
    panic_now: tokio::sync::Notify,
    settling: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl CommandExecutor for PanicDuringNative {
    async fn settle_effects(&self) {
        self.settling.notify_one();
        self.release.notified().await;
        self.executor.settle_effects().await;
    }

    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        tokio::select! {
            result = self.executor.run(request, cancellation, output) => result,
            () = self.panic_now.notified() => panic!("panic with a live native child"),
        }
    }
}

#[derive(Default)]
struct DelayedNativeCleanup {
    executor: TokioCommandExecutor,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    finished: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl CommandExecutor for DelayedNativeCleanup {
    async fn settle_effects(&self) {
        self.executor.settle_effects().await;
    }
    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        let native_cancellation = CancellationToken::default();
        let command = self
            .executor
            .run(request, native_cancellation.clone(), output);
        tokio::pin!(command);
        tokio::select! {
            result = &mut command => result,
            () = cancellation.cancelled() => {
                self.started.notify_one();
                self.release.notified().await;
                native_cancellation.cancel();
                let result = command.await;
                self.finished.store(true, std::sync::atomic::Ordering::Release);
                result
            }
        }
    }
}

struct BlockingExecutor;

#[async_trait]
impl CommandExecutor for BlockingExecutor {
    async fn settle_effects(&self) {}
    async fn run(
        &self,
        _request: CommandRequest,
        cancellation: CancellationToken,
        _output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        cancellation.cancelled().await;
        Err(ToolError::Cancelled)
    }
}

#[cfg(unix)]
#[test]
fn lease_descriptor_probe_subprocess_helper() {
    use std::os::unix::fs::MetadataExt as _;

    let Some(descriptor) = std::env::var_os("ROTTWEILER_LEASE_PROBE_FD") else {
        return;
    };
    let expected_device = std::env::var("ROTTWEILER_LEASE_PROBE_DEV")
        .expect("expected lease device")
        .parse::<u64>()
        .expect("numeric lease device");
    let expected_inode = std::env::var("ROTTWEILER_LEASE_PROBE_INO")
        .expect("expected lease inode")
        .parse::<u64>()
        .expect("numeric lease inode");
    let inherited = std::fs::metadata(format!("/dev/fd/{}", descriptor.to_string_lossy()))
        .is_ok_and(|metadata| {
            metadata.dev() == expected_device && metadata.ino() == expected_inode
        });
    if inherited {
        std::process::exit(90);
    }
}

#[cfg(unix)]
#[test]
fn watchdog_subprocess_helper() {
    if std::env::var_os("ROTTWEILER_WATCHDOG_HELPER").is_none() {
        return;
    }
    let ready = std::env::var("ROTTWEILER_WATCHDOG_READY").expect("ready path");
    let sentinel = std::env::var("ROTTWEILER_WATCHDOG_SENTINEL").expect("sentinel path");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("helper runtime");
    let executor = match std::env::var_os("ROTTWEILER_WATCHDOG_LEASE") {
        Some(path) => TokioCommandExecutor::with_execution_lease(Arc::new(
            ExecutionLease::acquire(path).expect("helper execution lease"),
        )),
        None => TokioCommandExecutor::default(),
    };
    runtime
            .block_on(executor.run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command: "printf '%s\\n' \"$$\" > \"$ROTTWEILER_WATCHDOG_READY\"; sleep 2; printf survived > \"$ROTTWEILER_WATCHDOG_SENTINEL\"; sleep 30".to_owned(),
                    cwd: std::env::temp_dir(),
                    env: BTreeMap::from([
                        ("ROTTWEILER_WATCHDOG_READY".to_owned(), ready),
                        ("ROTTWEILER_WATCHDOG_SENTINEL".to_owned(), sentinel),
                    ]),
                },
                CancellationToken::default(),
                Arc::new(crate::NoopOutputSink),
            ))
            .expect("helper command");
}

#[cfg(unix)]
async fn wait_for_test_file(helper: &mut Child, path: &std::path::Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        assert!(
            helper.try_wait().expect("helper status").is_none(),
            "helper exited before {} was created",
            path.display()
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "helper readiness timeout"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
async fn read_test_pid(path: &std::path::Path) -> rustix::process::Pid {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let raw = loop {
        match tokio::fs::read_to_string(path).await {
            Ok(value) => {
                if let Ok(pid) = value.trim().parse::<i32>() {
                    break pid;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("read pid file: {error}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "numeric pid was not published before the deadline"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    rustix::process::Pid::from_raw(raw).expect("positive pid")
}

#[cfg(target_os = "linux")]
fn test_process_is_running(pid: rustix::process::Pid) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero())) else {
        return false;
    };
    // The watchdog is orphaned when its executor is SIGKILLed. Linux can
    // retain the exited process as a zombie until PID 1 reaps it, during
    // which time kill(pid, 0) still reports success. The state is the first
    // field after the final ')' because comm may contain spaces.
    let Some((_, fields)) = stat.rsplit_once(") ") else {
        return false;
    };
    !fields.starts_with('Z')
}

#[cfg(all(unix, not(target_os = "linux")))]
fn test_process_is_running(pid: rustix::process::Pid) -> bool {
    rustix::process::test_kill_process(pid).is_ok()
}

mod lifecycle;
mod output;
mod replay;
mod safety;
