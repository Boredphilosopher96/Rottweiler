#![allow(clippy::expect_used)]
use super::*;
use std::{process::Stdio, time::Instant};

const ISOLATED: &str = "RW_REJECTED_HELPER_DIAGNOSTIC_PROBE";
const EXIT_TEST: &str = "plugin_process::rejected_helper::tests::rejected_helper_preserves_exit_status_and_stderr_after_actual_reap";

#[tokio::test]
async fn rejected_helper_preserves_exit_status_and_stderr_after_actual_reap() {
    let result = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, || {
        if std::env::var_os(ISOLATED).is_none() {
            isolated_exit_probe();
            return;
        }
        let _process = super::super::process_fixture_lease();
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "printf 'helper-bootstrap-canary' >&2; exit 23"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("fixture helper");
        let pid = child.id().expect("pid");
        let before = await_exit(pid);
        let error = retire(&mut child, pid, &io::Error::other("handshake refused"));
        assert_eq!(
            before
                .expect("exit observation")
                .expect("helper exit")
                .code(),
            Some(23)
        );
        assert!(
            error.message.contains("reaped=exit status: 23"),
            "{error:?}"
        );
        assert!(
            error.message.contains("stderr=helper-bootstrap-canary"),
            "{error:?}"
        );
        assert!(
            observe_exit(pid).is_err(),
            "wait proof was consumed by retirement"
        );
    })
    .await;
    result.expect("owned diagnostic worker");
}

fn isolated_exit_probe() {
    use rw_resources::process::BlockingProcess;
    use std::io::{Read, Seek};

    // Parallel process creation can temporarily inherit a pipe writer between
    // fork and exec. Reaping this helper alone does not establish pipe EOF.
    // Give the complete-capture case its own process and descriptor inventory.
    let mut output = tempfile::tempfile().expect("probe diagnostics");
    let mut probe = BlockingProcess::spawn(
        std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([EXIT_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(ISOLATED, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().expect("probe stdout")))
            .stderr(Stdio::from(output.try_clone().expect("probe stderr"))),
    )
    .expect("isolated diagnostic probe");
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match probe.try_status() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            observed => break Err(format!("probe did not finish: {observed:?}")),
        }
    };
    probe.settle();
    output.rewind().expect("rewind diagnostics");
    let mut diagnostic = String::new();
    output
        .take(64 * 1024)
        .read_to_string(&mut diagnostic)
        .expect("bounded diagnostics");
    assert!(
        status.as_ref().is_ok_and(|status| status.success()),
        "{status:?}\n{diagnostic}"
    );
}

fn await_exit(pid: u32) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = observe_exit(pid);
        if !matches!(status, Ok(None)) || Instant::now() >= deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[tokio::test]
async fn reaped_helper_with_retained_stderr_writer_omits_incomplete_capture() {
    let result = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, || {
        use std::os::fd::OwnedFd;
        let _process = super::super::process_fixture_lease();
        let (read, writer) = std::os::unix::net::UnixStream::pair().expect("stderr descriptors");
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "printf 'partial-credential' >&2; exit 23"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(OwnedFd::from(
                writer.try_clone().expect("child writer"),
            )))
            .kill_on_drop(true)
            .spawn()
            .expect("fixture helper");
        child.stderr = Some(
            tokio::process::ChildStderr::from_std(std::process::ChildStderr::from(OwnedFd::from(
                read,
            )))
            .expect("nonblocking stderr"),
        );
        let pid = child.id().expect("pid");
        let before = await_exit(pid);
        let error = retire(&mut child, pid, &io::Error::other("handshake refused"));
        assert_eq!(
            before
                .expect("exit observation")
                .expect("helper exit")
                .code(),
            Some(23)
        );
        assert!(
            error.message.contains("reaped=exit status: 23"),
            "{error:?}"
        );
        assert!(error.message.contains("content omitted"), "{error:?}");
        assert!(!error.message.contains("partial-credential"), "{error:?}");
        drop(writer);
    })
    .await;
    result.expect("owned incomplete diagnostic worker");
}

#[test]
fn incomplete_stderr_never_exposes_a_partial_credential() {
    let (read, write) = std::os::unix::net::UnixStream::pair().expect("diagnostic pipe");
    read.set_nonblocking(true)
        .expect("nonblocking diagnostic pipe");
    rustix::io::write(&write, b"partial-secret").expect("fixture bytes");
    assert!(!read_stderr(&read).contains("partial-secret"));
    drop(write);
}

#[test]
fn oversized_stderr_is_omitted_with_finite_capture() {
    let (read, write) = std::os::unix::net::UnixStream::pair().expect("diagnostic pipe");
    read.set_nonblocking(true)
        .expect("nonblocking diagnostic pipe");
    let bytes = [b'x'; STDERR_BYTES];
    assert_eq!(
        rustix::io::write(&write, &bytes).expect("fixture bytes"),
        bytes.len()
    );
    drop(write);
    assert!(read_stderr(&read).contains("content omitted"));
}
