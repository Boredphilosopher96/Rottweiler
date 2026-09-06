//! PTY creation and IO pump setup run under owned physical admission.
use super::{
    CapturedTail, PtySignalTarget, TerminalModeGuard, TerminalSpawner, TokioTerminalChild,
    TokioTerminalSpawner,
    ownership::{self, ProcessOwner},
    pump_terminal_output, real_terminal_size, spawn_terminal_input_thread,
};
use async_trait::async_trait;
use portable_pty::{CommandBuilder, native_pty_system};
use std::{
    ffi::OsString,
    io,
    sync::{Arc, Mutex},
    thread,
};
struct SpawnFailure {
    error: io::Error,
    process: Option<Box<ProcessOwner>>,
}
impl From<io::Error> for SpawnFailure {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            process: None,
        }
    }
}
#[async_trait]
impl TerminalSpawner for TokioTerminalSpawner {
    type Child = TokioTerminalChild;
    async fn spawn_tty(&self, argv: &[OsString]) -> io::Result<Self::Child> {
        if argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
        }
        let credit =
            rw_resources::acquire(rw_resources::ResourceClass::Process, std::future::pending())
                .await
                .map_err(io::Error::other)?;
        let argv = argv.to_vec();
        let spawner = Self {
            pump_terminal_input: self.pump_terminal_input,
            intercept_interrupt_input: self.intercept_interrupt_input,
        };
        match rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            spawner.spawn_owned(argv, credit)
        })
        .await
        .map_err(|error| match error {
            rw_resources::WorkError::Admission(error) => io::Error::other(error),
            rw_resources::WorkError::Worker(error) => {
                ownership::unsettled(format!("PTY creation worker lost proof: {error}"))
            }
        })? {
            Ok(child) => Ok(child),
            Err(mut failure) => {
                if let Some(process) = failure.process.as_mut() {
                    process.cancel();
                    if let Err(error) = process.wait().await
                        && ownership::is_unsettled(&error)
                    {
                        return Err(error);
                    }
                }
                Err(failure.error)
            }
        }
    }
}
impl TokioTerminalSpawner {
    fn spawn_owned(
        &self,
        argv: Vec<OsString>,
        credit: rw_resources::ResourceLease,
    ) -> Result<TokioTerminalChild, SpawnFailure> {
        let pair = native_pty_system()
            .openpty(real_terminal_size())
            .map_err(io::Error::other)?;
        let mut command = CommandBuilder::from_argv(argv);
        command.set_controlling_tty(true);
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        let mut process = ProcessOwner::new(child, credit);
        drop(pair.slave);
        let setup = (|| {
            let physical = process.physical()?;
            let group = pair
                .master
                .process_group_leader()
                .and_then(rustix::process::Pid::from_raw)
                .or(physical.group)
                .ok_or_else(|| ownership::unsettled("PTY child has no process group"))?;
            physical.group = Some(group);
            physical.group_known = true;
            let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
            let writer = pair.master.take_writer().map_err(io::Error::other)?;
            let master = Arc::new(Mutex::new(pair.master));
            let target = PtySignalTarget {
                process_group: group,
                master,
                active: physical.active.clone(),
            };
            let captured_tail = Arc::new(Mutex::new(CapturedTail::default()));
            *physical
                .terminal_mode
                .lock()
                .map_err(|_| io::Error::other("terminal mode owner poisoned"))? =
                TerminalModeGuard::enter()?;
            if self.pump_terminal_input {
                physical.input = Some(spawn_terminal_input_thread(
                    writer,
                    physical.cancelled.clone(),
                    target.clone(),
                    self.intercept_interrupt_input,
                )?);
            } else {
                physical.idle_writer = Some(writer);
            }
            let output_tail = captured_tail.clone();
            physical.output = Some(
                thread::Builder::new()
                    .name("rw-pty-output".to_owned())
                    .spawn(move || pump_terminal_output(&mut reader, &output_tail))?,
            );
            Ok::<_, io::Error>((target, captured_tail, physical.terminal_mode.clone()))
        })();
        match setup {
            Ok((target, captured_tail, terminal_mode)) => Ok(TokioTerminalChild {
                process,
                target,
                captured_tail,
                terminal_mode,
            }),
            Err(error) => Err(SpawnFailure {
                error,
                process: Some(Box::new(process)),
            }),
        }
    }
}
