#![allow(clippy::expect_used)]
use super::*;
use std::{process::Stdio, time::Instant};

#[tokio::test]
async fn rejected_helper_preserves_exit_status_and_stderr_after_actual_reap() {
    let result = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, || {
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
        let deadline = Instant::now() + Duration::from_secs(2);
        let before = loop {
            let status = observe_exit(pid);
            if !matches!(status, Ok(None)) || Instant::now() >= deadline {
                break status;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let error = retire(&mut child, pid, &io::Error::other("handshake refused"));
        assert_eq!(
            before
                .expect("exit observation")
                .expect("helper exit")
                .code(),
            Some(23)
        );
        assert!(error.message.contains("reaped=exit status: 23"));
        assert!(error.message.contains("stderr=helper-bootstrap-canary"));
        assert!(
            observe_exit(pid).is_err(),
            "wait proof was consumed by retirement"
        );
    })
    .await;
    result.expect("owned diagnostic worker");
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
