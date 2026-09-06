//! Actual print clients must settle even when their physical stdout never drains.
use super::*;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::os::fd::OwnedFd;

fn full_pipe() -> (OwnedFd, OwnedFd) {
    let (read, write) = rustix::pipe::pipe().expect("stdout pipe");
    let flags = fcntl_getfl(&write).expect("flags");
    fcntl_setfl(&write, flags | OFlags::NONBLOCK).expect("fill nonblocking");
    loop {
        match rustix::io::write(&write, &[b'x'; 4096]) {
            Ok(_) => {}
            Err(rustix::io::Errno::AGAIN) => break,
            Err(error) => panic!("fill output: {error}"),
        }
    }
    fcntl_setfl(&write, flags).expect("original blocking mode");
    (read, write)
}

#[test]
fn print_modes_sigint_settles_with_full_stdout_and_leaves_stdin_untouched() {
    for format in ["text", "stream-json", "json"] {
        blocked_print(format);
    }
}

fn blocked_print(format: &str) {
    let root = tempdir().expect("root");
    let run = TestRun::new(&root, format);
    let fixtures = root.path().join("fixtures");
    let script = root.path().join("script.json");
    write_script(
        &script,
        vec![vec![
            ProviderEvent::TextDelta {
                text: "ready".repeat(32 * 1024),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]],
    );
    let (blocked_read, blocked_write) = full_pipe();
    let original_output_flags = fcntl_getfl(&blocked_write).expect("output flags");
    let observed_output = rustix::io::dup(&blocked_write).expect("observe shared flags");
    let (input_read, input_write) = rustix::pipe::pipe().expect("stdin pipe");
    let observed_input = rustix::io::dup(&input_read).expect("observe stdin");
    let original_input_flags = fcntl_getfl(&observed_input).expect("input flags");
    rustix::io::write(&input_write, b"unconsumed input\n").expect("stdin bytes");
    let mut child = TestProcess::spawn(
        base_command(&run.workspace, &run.home)
            .args([
                "-p",
                "print bounded output",
                "--permission-mode",
                "yolo",
                "--output-format",
                format,
                "--replay-dir",
                fixtures.to_str().expect("fixtures"),
                "--record-replay-script",
                script.to_str().expect("script"),
            ])
            .stdin(Stdio::from(input_read))
            .stdout(Stdio::from(blocked_write)),
    );
    child.wait_ready(|| {
        event_log(&run.home)
            .and_then(|p| fs::read_to_string(p).ok())
            .is_some_and(|source| source.contains("turn_finished"))
    });
    assert!(child.child.try_wait().expect("blocked process").is_none());
    assert_eq!(
        fcntl_getfl(&observed_input).expect("unmodified stdin"),
        original_input_flags
    );
    assert!(
        Command::new("kill")
            .args(["-INT", &child.child.id().to_string()])
            .status()
            .expect("SIGINT")
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.child.try_wait().expect("settled process") {
            assert!(
                !status.success(),
                "interrupted output must not claim success"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "print {format} did not settle while stdout was full"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fcntl_getfl(&observed_output).expect("restored stdout"),
        original_output_flags
    );
    let mut bytes = [0; 17];
    assert_eq!(
        rustix::io::read(&observed_input, &mut bytes).expect("stdin retained"),
        17
    );
    assert_eq!(&bytes, b"unconsumed input\n");
    drop(blocked_read);
}
