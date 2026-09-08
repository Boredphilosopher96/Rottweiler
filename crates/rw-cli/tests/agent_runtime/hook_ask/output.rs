//! Bounded pipe capture with exit observation before physical group retirement.
use rw_resources::process::BlockingProcess;
use std::{
    io::{self, Read},
    os::fd::AsFd,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const STDOUT_LIMIT: usize = 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

pub(super) fn bounded_output(command: &mut Command) -> Vec<u8> {
    capture(command).expect("owned bounded fixture process")
}

fn capture(command: &mut Command) -> io::Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = BlockingProcess::spawn(command)?;
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let result = (|| {
        let mut pipes = process.take_pipes()?;
        let mut stdout = pipes
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let mut stderr = pipes
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr"))?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        read_output(
            &process,
            &mut stdout,
            &mut stderr,
            &mut output,
            &mut diagnostics,
        )
    })();
    // Even an overflow/deadline error retains reaping authority through final
    // group signalling. No assertion or returned diagnostic precedes settlement.
    process.settle();
    match result {
        Ok(status) if status.success() => Ok(output),
        result => Err(io::Error::other(format!(
            "fixture process failed: {result:?}; stderr: {}",
            String::from_utf8_lossy(&diagnostics)
        ))),
    }
}

fn nonblocking(pipe: &impl AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(pipe)?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

fn read_output(
    process: &BlockingProcess,
    stdout: &mut impl Read,
    stderr: &mut impl Read,
    output: &mut Vec<u8>,
    diagnostics: &mut Vec<u8>,
) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let stdout_closed = drain(stdout, output, STDOUT_LIMIT)?;
        let stderr_closed = drain(stderr, diagnostics, STDERR_LIMIT)?;
        if let Some(status) = process.try_status()?
            && stdout_closed
            && stderr_closed
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fixture process deadline",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut scratch = [0; 8192];
    // A hot producer cannot starve the other pipe or the absolute deadline.
    for _ in 0..16 {
        match pipe.read(&mut scratch) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if count > limit.saturating_sub(bytes.len()) {
                    return Err(io::Error::other("fixture output exceeded its byte limit"));
                }
                bytes.extend_from_slice(&scratch[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[test]
fn streaming_overflow_settles_infinite_producer_on_either_pipe() {
    for redirect in ["", " >&2"] {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            &format!("while :; do printf '%8192s' x{redirect}; done"),
        ]);
        let error = capture(&mut command).expect_err("live producer exceeds capture ceiling");
        assert!(error.to_string().contains("byte limit"));
        assert!(error.to_string().len() <= STDERR_LIMIT + 256);
    }
}
