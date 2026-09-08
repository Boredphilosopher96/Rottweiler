#![allow(clippy::expect_used)]
use super::*;
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::{Notify, Semaphore},
};

struct Fixture {
    _root: tempfile::TempDir,
    paths: ServerRuntimePaths,
    pid: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let directory = root.path().join("runtime");
        fs::create_dir(&directory).expect("runtime directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("private runtime");
        Self {
            paths: ServerRuntimePaths {
                socket: directory.join("engine.sock"),
                token: directory.join("auth.token"),
                descriptor: directory.join("runtime.json"),
                directory,
            },
            pid: root.path().join("pid"),
            _root: root,
        }
    }
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf %s \"$$\" > \"$1\"; exec sleep 30",
                "detached-fixture",
            ])
            .arg(&self.pid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
    async fn process_id(&self) -> rustix::process::Pid {
        bounded(async {
            loop {
                if let Ok(text) = fs::read_to_string(&self.pid)
                    && let Ok(pid) = text.parse::<i32>()
                    && let Some(pid) = rustix::process::Pid::from_raw(pid)
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
    }
    fn start<F: Future<Output = Result<()>> + Send + 'static>(
        &self,
        deliver: impl FnOnce(DetachedServerReady, Stop) -> F + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let command = self.command();
        let paths = self.paths.clone();
        tokio::spawn(crate::tui_session::own(move |stop| {
            Box::pin(start(command, paths, "session".into(), stop, deliver))
        }))
    }
    fn health(&self) -> tokio::task::JoinHandle<()> {
        let (runtime, listener) =
            crate::server::ServerRuntime::create_for_session(self.paths.clone(), Some("session"))
                .expect("published private runtime");
        let listener = tokio::net::UnixListener::from_std(listener).expect("async listener");
        let token = fs::read_to_string(&runtime.paths.token).expect("private token");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                loop {
                    let byte = stream.read_u8().await.expect("health request");
                    request.push(byte);
                    assert!(request.len() < 8192);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                assert!(request.starts_with(b"GET /v1/health "));
                assert!(
                    String::from_utf8_lossy(&request)
                        .to_ascii_lowercase()
                        .contains(&format!("authorization: bearer {}", token.trim()))
                );
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await
                    .expect("health response");
            }
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Failed assertions must not leave the real fixture engine running.
        if let Ok(text) = fs::read_to_string(&self.pid)
            && let Ok(pid) = text.parse::<i32>()
            && let Some(pid) = rustix::process::Pid::from_raw(pid)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}
async fn bounded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .expect("fixture transition")
}
async fn retired(pid: rustix::process::Pid) {
    bounded(async {
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
}

#[tokio::test]
async fn lost_startup_waiter_reaps_unannounced_child() {
    let fixture = Fixture::new();
    let announced = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&announced);
    let waiter = fixture.start(move |_, _| async move {
        observed.store(true, Ordering::SeqCst);
        Ok(())
    });
    let pid = fixture.process_id().await;
    waiter.abort();
    assert!(waiter.await.expect_err("lost caller").is_cancelled());
    retired(pid).await;
    assert!(!announced.load(Ordering::SeqCst));
}

#[tokio::test]
async fn failed_readiness_write_reaps_ready_child_before_return() {
    let fixture = Fixture::new();
    let (mut output, reader) = tokio::net::UnixStream::pair().expect("readiness transport");
    drop(reader);
    let waiter = fixture.start(move |ready, _| async move {
        assert!(ready.started);
        output.write_all(b"ready\n").await.into_diagnostic()
    });
    let pid = fixture.process_id().await;
    let health = fixture.health();
    assert!(bounded(waiter).await.expect("startup task").is_err());
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    health.abort();
    let _ = health.await;
}

#[tokio::test]
async fn cancellation_during_readiness_handoff_reaps_without_transfer() {
    let fixture = Fixture::new();
    let entered = Arc::new(Notify::new());
    let observed = Arc::clone(&entered);
    let waiter = fixture.start(move |_, mut stop| async move {
        observed.notify_one();
        stop.cancelled().await;
        Err(miette!("readiness receiver disappeared"))
    });
    let pid = fixture.process_id().await;
    let health = fixture.health();
    bounded(entered.notified()).await;
    assert!(rustix::process::test_kill_process(pid).is_ok());
    waiter.abort();
    let _ = waiter.await;
    retired(pid).await;
    health.abort();
    let _ = health.await;
}

#[tokio::test]
async fn acknowledged_readiness_intentionally_detaches_engine() {
    let fixture = Fixture::new();
    let entered = Arc::new(Notify::new());
    let observed = Arc::clone(&entered);
    let release = Arc::new(Semaphore::new(0));
    let delivery = Arc::clone(&release);
    let waiter = fixture.start(move |ready, _| async move {
        assert!(ready.started);
        assert_eq!(ready.session_id, "session");
        assert!(runtime_paths::valid_bootstrap_token(&ready.token));
        observed.notify_one();
        delivery
            .acquire()
            .await
            .expect("delivery acknowledgement")
            .forget();
        Ok(())
    });
    let pid = fixture.process_id().await;
    let health = fixture.health();
    bounded(entered.notified()).await;
    assert!(
        !waiter.is_finished(),
        "readiness still owned before output acknowledgement"
    );
    release.add_permits(1);
    bounded(waiter)
        .await
        .expect("owner task")
        .expect("readiness transfer");
    assert!(
        rustix::process::test_kill_process(pid).is_ok(),
        "announced engine stays alive after caller returns"
    );
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("fixture cleanup");
    retired(pid).await;
    health.abort();
    let _ = health.await;
}

#[tokio::test]
async fn oversized_readiness_rejection_reaps_before_any_output_owner_starts() {
    let fixture = Fixture::new();
    let waiter = fixture.start(|mut ready, stop| async move {
        ready.session_id = "s".repeat(MAX_ANNOUNCEMENT_BYTES);
        announce(ready, stop).await
    });
    let pid = fixture.process_id().await;
    let health = fixture.health();
    let error = bounded(waiter)
        .await
        .expect("owner task")
        .expect_err("bounded JSON output");
    assert!(error.to_string().contains("JSON encoded byte limit"));
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    health.abort();
    let _ = health.await;
}

#[tokio::test]
async fn existing_live_engine_is_not_owned_by_a_failed_readiness_recipient() {
    let fixture = Fixture::new();
    let health = fixture.health();
    let waiter = fixture.start(|ready, _| async move {
        assert!(!ready.started);
        Err(miette!("readiness recipient closed"))
    });
    assert!(bounded(waiter).await.expect("owner task").is_err());
    assert!(!fixture.pid.exists(), "no new engine was spawned");
    assert!(runtime_paths::runtime_is_live(&fixture.paths).await);
    health.abort();
    let _ = health.await;
}
