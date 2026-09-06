#![allow(clippy::expect_used)]
use crate::tty::{
    OutputRedactor, ShellCompletionGate, SignalTarget, TerminalChild, TerminalExit, TerminalSignal,
    TerminalSignalSource, TerminalSpawner, TokioTerminalSpawner,
    run_argv_after_durable_shell_start,
};
use async_trait::async_trait;
use std::{
    ffi::OsString,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[tokio::test]
async fn dropped_wait_retains_process_credit_until_actual_pty_reap() {
    const MARKER: &str = "RW_PTY_RESOURCE_PROOF_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tty::ownership::tests::dropped_wait_retains_process_credit_until_actual_pty_reap",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .status()
            .expect("isolated process pool test");
        assert!(status.success());
        return;
    }
    let spawner = TokioTerminalSpawner::without_terminal_input();
    let mut child = spawner
        .spawn_tty(&[
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("exec sleep 30"),
        ])
        .await
        .expect("real PTY child");
    let target = child.signal_target();
    let mut held = Vec::new();
    while let Ok(lease) = rw_resources::try_acquire(rw_resources::ResourceClass::Process) {
        held.push(lease);
    }
    assert_eq!(held.len(), 63, "actual PTY owns one global process slot");
    let waiter = tokio::spawn(async move { child.wait().await });
    tokio::task::yield_now().await;
    waiter.abort();
    assert!(waiter.await.expect_err("caller cancelled").is_cancelled());
    let released = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(lease) = rw_resources::try_acquire(rw_resources::ResourceClass::Process) {
                break lease;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual cleanup releases slot");
    assert!(matches!(
        rustix::process::test_kill_process_group(target.process_group),
        Err(rustix::io::Errno::SRCH)
    ));
    assert!(!*target.active.lock().expect("signal state"));
    target
        .forward(TerminalSignal::Interrupt)
        .expect("retired signal target is inert");
    drop(released);
}

struct Gate(Arc<AtomicUsize>);
#[async_trait]
impl ShellCompletionGate for Gate {
    async fn shell_ended(&self, _: rw_core::ShellId, _: i32, _: Option<String>) -> io::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[derive(Clone)]
struct Target;
impl SignalTarget for Target {
    fn forward(&self, _: TerminalSignal) -> io::Result<()> {
        Ok(())
    }
}
struct Signals;
#[async_trait]
impl TerminalSignalSource for Signals {
    async fn recv(&mut self) -> io::Result<TerminalSignal> {
        std::future::pending().await
    }
}
struct Redactor;
impl OutputRedactor for Redactor {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}
struct FailedChild;
#[async_trait]
impl TerminalChild for FailedChild {
    type Target = Target;
    fn signal_target(&self) -> Target {
        Target
    }
    async fn wait(&mut self) -> io::Result<TerminalExit> {
        Err(super::unsettled("actual effects remain"))
    }
}
struct FailedSpawner(bool);
#[async_trait]
impl TerminalSpawner for FailedSpawner {
    type Child = FailedChild;
    async fn spawn_tty(&self, _: &[OsString]) -> io::Result<FailedChild> {
        if self.0 {
            Err(super::unsettled("spawn cleanup remains"))
        } else {
            Ok(FailedChild)
        }
    }
}
#[tokio::test]
async fn unproven_spawn_or_wait_never_publishes_shell_ended() {
    for during_spawn in [false, true] {
        let ended = Arc::new(AtomicUsize::new(0));
        let result = run_argv_after_durable_shell_start(
            &[OsString::from("shell")],
            rw_core::ShellId("owned-shell".to_owned()),
            &Gate(ended.clone()),
            &FailedSpawner(during_spawn),
            &mut Signals,
            &Redactor,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(ended.load(Ordering::SeqCst), 0);
    }
}

#[derive(Debug)]
struct DelayedWait {
    child: std::process::Child,
    unavailable: Arc<std::sync::atomic::AtomicBool>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}
impl portable_pty::ChildKiller for DelayedWait {
    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        portable_pty::ChildKiller::clone_killer(&self.child)
    }
}
impl portable_pty::Child for DelayedWait {
    fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "controlled wait permission",
            ));
        }
        self.child.try_wait().map(|status| status.map(Into::into))
    }
    fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
        self.child.wait().map(Into::into)
    }
    fn process_id(&self) -> Option<u32> {
        Some(self.child.id())
    }
}
impl Drop for DelayedWait {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn transient_failed_proof_preserves_actual_owner_until_later_reap() {
    use std::{os::unix::process::CommandExt as _, sync::atomic::AtomicBool};
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "exec sleep 30"])
        .process_group(0)
        .spawn()
        .expect("real child");
    let group = rustix::process::Pid::from_raw(i32::try_from(child.id()).expect("PID range"))
        .expect("group");
    let unavailable = Arc::new(AtomicBool::new(true));
    let dropped = Arc::new(AtomicBool::new(false));
    let credit =
        rw_resources::try_acquire(rw_resources::ResourceClass::Process).expect("process credit");
    let mut owner = super::ProcessOwner::new(
        Box::new(DelayedWait {
            child,
            unavailable: unavailable.clone(),
            dropped: dropped.clone(),
        }),
        credit,
    );
    let error = owner.wait().await.expect_err("typed missing proof");
    assert!(super::is_unsettled(&error));
    assert!(
        !dropped.load(Ordering::SeqCst),
        "failed proof retains actual child"
    );
    unavailable.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("eventual proof retires actual owner");
    assert!(matches!(
        rustix::process::test_kill_process_group(group),
        Err(rustix::io::Errno::SRCH)
    ));
}
