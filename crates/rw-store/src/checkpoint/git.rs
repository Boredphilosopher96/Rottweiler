//! Bounded Git pipes; the child remains owned through cancellation and reaping.
use super::operation::CheckpointCancellation;
use super::{CheckpointError, CheckpointOperation};
use std::{
    io::{self, Read},
    process::{ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) struct GitPipe {
    child: rw_resources::process::BlockingProcess,
    stdout: ChildStdout,
    cancellation: CheckpointCancellation,
    deadline: Instant,
}

impl GitPipe {
    pub(super) fn spawn(
        command: &mut Command,
        operation: &CheckpointOperation,
    ) -> io::Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = rw_resources::process::BlockingProcess::spawn(command)?;
        let Some(stdout) = child.child_mut()?.stdout.take() else {
            child.settle();
            return Err(io::Error::other("Git stdout was not captured"));
        };
        let pipe = Self {
            child,
            stdout,
            cancellation: operation.cancellation(),
            deadline: operation.deadline(),
        };
        #[cfg(unix)]
        {
            use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
            let flags = fcntl_getfl(&pipe.stdout)?;
            fcntl_setfl(&pipe.stdout, flags | OFlags::NONBLOCK)?;
        }
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bounded Git pipes require Unix",
        ));
        #[cfg(unix)]
        Ok(pipe)
    }

    fn check(&self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::other("checkpoint cancelled"));
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "checkpoint deadline",
            ));
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> io::Result<bool> {
        loop {
            self.check()?;
            if let Some(status) = self.child.child_mut()?.try_wait()? {
                self.child.settle();
                return Ok(status.success());
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Read for GitPipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            match self.stdout.read(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                result => return result,
            }
        }
    }
}

pub(super) fn query(
    command: &mut Command,
    operation: &CheckpointOperation,
) -> Result<Option<Vec<u8>>, CheckpointError> {
    const MAX_OUTPUT: usize = 16 * 1024 * 1024;
    operation.check()?;
    let mut pipe = match GitPipe::spawn(command, operation) {
        Ok(pipe) => pipe,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    let result = (&mut pipe)
        .take(MAX_OUTPUT as u64 + 1)
        .read_to_end(&mut bytes);
    operation.check()?;
    result?;
    if bytes.len() > MAX_OUTPUT {
        return Err(CheckpointError::OperationLimit("16 MiB Git output"));
    }
    Ok(pipe.finish()?.then_some(bytes))
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn cancellation_interrupts_a_blocked_pipe_and_reaps_the_child() {
        let operation = CheckpointOperation::default();
        let mut pipe = GitPipe::spawn(
            Command::new("sh").args(["-c", "printf x; exec sleep 30"]),
            &operation,
        )
        .expect("child");
        let pid = rustix::process::Pid::from_raw(
            i32::try_from(pipe.child.child_mut().expect("owned child").id()).expect("native pid"),
        )
        .expect("pid");
        pipe.read_exact(&mut [0]).expect("child started");
        operation.cancellation().cancel();
        assert!(pipe.read(&mut [0]).is_err());
        drop(pipe);
        assert!(matches!(
            rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
            Err(rustix::io::Errno::CHILD)
        ));
    }

    #[test]
    fn excessive_stdout_is_bounded_and_the_producer_is_terminated() {
        let operation = CheckpointOperation::default();
        let error =
            query(Command::new("sh").args(["-c", "exec yes x"]), &operation).expect_err("limit");
        assert!(matches!(
            error,
            CheckpointError::OperationLimit("16 MiB Git output")
        ));
    }
}
