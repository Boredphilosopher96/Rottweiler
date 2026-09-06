//! Descriptor flags and terminal mode belong to the physical polling worker.
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    termios::{self, OptionalActions, Termios},
};
use std::{
    io,
    os::fd::{AsFd, OwnedFd},
};

pub(super) struct TerminalIo {
    pub input: OwnedFd,
    pub output: OwnedFd,
    input_flags: OFlags,
    output_flags: OFlags,
    mode: Option<Termios>,
}

impl TerminalIo {
    pub fn open(input: OwnedFd, output: OwnedFd) -> io::Result<Self> {
        let input_flags = fcntl_getfl(&input)?;
        let output_flags = fcntl_getfl(&output)?;
        let mode = match termios::tcgetattr(&input) {
            Ok(mode) => Some(mode),
            Err(rustix::io::Errno::NOTTY) => None,
            Err(error) => return Err(error.into()),
        };
        let owner = Self {
            input,
            output,
            input_flags,
            output_flags,
            mode,
        };
        fcntl_setfl(&owner.input, input_flags | OFlags::NONBLOCK)?;
        fcntl_setfl(&owner.output, output_flags | OFlags::NONBLOCK)?;
        if let Some(original) = &owner.mode {
            let mut raw = original.clone();
            raw.make_raw();
            termios::tcsetattr(&owner.input, OptionalActions::Now, &raw)?;
        }
        Ok(owner)
    }
    pub fn interactive(&self) -> bool {
        self.mode.is_some()
    }
    pub fn restore(&mut self) -> io::Result<()> {
        let terminal = self.mode.take().map_or(Ok(()), |mode| {
            termios::tcsetattr(&self.input, OptionalActions::Flush, &mode)
        });
        let input = fcntl_setfl(&self.input, self.input_flags);
        let output = fcntl_setfl(&self.output, self.output_flags);
        terminal.and(input).and(output).map_err(Into::into)
    }
}
impl Drop for TerminalIo {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub(super) fn duplicate(fd: impl AsFd) -> io::Result<OwnedFd> {
    rustix::io::dup(fd).map_err(Into::into)
}
