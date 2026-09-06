//! One physical worker owns Git, its nonblocking pipes, and group settlement.
use std::{
    io::{self, Read, Write},
    process::{Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use rw_resources::{ResourceClass, process::BlockingProcess};

use super::{DIAGNOSTIC_LIMIT, MAX_GIT_OUTPUT_BYTES, git::GitOutput};
use crate::registry::{CancellationToken, ToolError};

struct Caller(CancellationToken);
impl Drop for Caller {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) async fn run(
    mut command: Command,
    input: Option<&[u8]>,
    cancellation: &CancellationToken,
) -> Result<GitOutput, ToolError> {
    let caller = Caller(CancellationToken::default());
    let abandoned = caller.0.clone();
    let cancellation = cancellation.clone();
    let input = input.unwrap_or_default().to_vec();
    rw_resources::run_blocking(ResourceClass::Blocking, move || {
        cancellation.check()?;
        abandoned.check()?;
        let mut process =
            BlockingProcess::spawn(&mut command).map_err(|error| command_error(&error))?;
        let result = communicate(&mut process, &input, &cancellation, &abandoned);
        // Every result, including pipe failures and cancellation, crosses the
        // same physical settlement barrier before the effect caller resumes.
        process.settle();
        result
    })
    .await
    .map_err(|error| ToolError::Command(format!("Git worker failed: {error}")))?
}

fn command_error(error: &io::Error) -> ToolError {
    ToolError::Command(format!("Git execution failed: {error}"))
}

fn nonblocking(pipe: &impl std::os::fd::AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(pipe)?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

fn communicate(
    process: &mut BlockingProcess,
    input: &[u8],
    cancellation: &CancellationToken,
    abandoned: &CancellationToken,
) -> Result<GitOutput, ToolError> {
    let child = process.child_mut().map_err(|error| command_error(&error))?;
    let mut stdin = child.stdin.take();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| command_error(&io::Error::other("missing stdout")))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| command_error(&io::Error::other("missing stderr")))?;
    nonblocking(&stdout).map_err(|error| command_error(&error))?;
    nonblocking(&stderr).map_err(|error| command_error(&error))?;
    if let Some(pipe) = stdin.as_ref() {
        nonblocking(pipe).map_err(|error| command_error(&error))?;
    }
    let mut written = 0;
    let mut output = Output::new(MAX_GIT_OUTPUT_BYTES);
    let mut diagnostic = Output::new(DIAGNOSTIC_LIMIT);
    let mut exited: Option<(ExitStatus, Instant)> = None;
    loop {
        cancellation.check()?;
        abandoned.check()?;
        let mut progress = output
            .read(&mut stdout)
            .map_err(|error| command_error(&error))?;
        progress |= diagnostic
            .read(&mut stderr)
            .map_err(|error| command_error(&error))?;
        if written == input.len() {
            stdin.take();
        }
        if let Some(pipe) = stdin.as_mut() {
            match pipe.write(&input[written..input.len().min(written + 16 * 1024)]) {
                Ok(0) => return Err(command_error(&io::ErrorKind::WriteZero.into())),
                Ok(count) => {
                    written += count;
                    progress = true;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(command_error(&error)),
            }
        }
        if exited.is_none()
            && let Some(status) = process
                .child_mut()
                .map_err(|error| command_error(&error))?
                .try_wait()
                .map_err(|error| command_error(&error))?
        {
            process.settle();
            exited = Some((status, Instant::now()));
        }
        if let Some((status, at)) = exited {
            if output.eof && diagnostic.eof {
                if output.truncated || diagnostic.truncated {
                    return Err(ToolError::SizeLimit {
                        limit: if output.truncated {
                            MAX_GIT_OUTPUT_BYTES
                        } else {
                            DIAGNOSTIC_LIMIT
                        },
                    });
                }
                return Ok(GitOutput {
                    status,
                    stdout: output.bytes,
                    stderr: diagnostic.bytes,
                });
            }
            if at.elapsed() >= Duration::from_secs(2) {
                return Err(command_error(&io::Error::other(
                    "Git output pipes did not close",
                )));
            }
        }
        if !progress {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

struct Output {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
    eof: bool,
}

impl Output {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            truncated: false,
            eof: false,
        }
    }
    fn read(&mut self, pipe: &mut impl Read) -> io::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let mut buffer = [0; 16 * 1024];
        match pipe.read(&mut buffer) {
            Ok(0) => {
                self.eof = true;
                Ok(true)
            }
            Ok(count) => {
                let retained = self.limit.saturating_sub(self.bytes.len()).min(count);
                self.bytes.extend_from_slice(&buffer[..retained]);
                self.truncated |= retained < count;
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::process::Stdio;

    fn command(script: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    #[tokio::test]
    async fn input_and_both_output_pipes_make_progress_without_detached_readers() {
        let input = vec![b'x'; 128 * 1024];
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            run(
                command("printf diagnostic >&2; cat"),
                Some(&input),
                &CancellationToken::default(),
            ),
        )
        .await
        .expect("multiplexed progress")
        .expect("output");
        assert!(output.status.success());
        assert_eq!(output.stdout, input);
        assert_eq!(output.stderr, b"diagnostic");
    }

    #[tokio::test]
    async fn dropping_the_caller_cancels_and_reaps_its_physical_worker() {
        let directory = tempfile::tempdir().expect("directory");
        let marker = directory.path().join("pid");
        let mut command = command("printf '%s' \"$$\" > \"$1\"; exec sleep 30");
        command.arg("git-worker-test").arg(&marker);
        let caller =
            tokio::spawn(async move { run(command, None, &CancellationToken::default()).await });
        let id: i32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&marker)
                    && let Ok(id) = value.parse()
                {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("child started");
        caller.abort();
        assert!(caller.await.expect_err("cancelled caller").is_cancelled());
        let pid = rustix::process::Pid::from_raw(id).expect("pid");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    rustix::process::test_kill_process_group(pid),
                    Err(rustix::io::Errno::SRCH)
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("owned worker reaped group after caller loss");
        assert!(matches!(
            rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
            Err(rustix::io::Errno::CHILD)
        ));
    }
}
