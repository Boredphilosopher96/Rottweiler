use super::super::input::InputSender;
use super::{
    OutputRequest, Wake,
    io::TerminalIo,
    lines::{Lines, MAX_ECHO_BYTES},
};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::{
    io::{self, Read},
    os::fd::{AsFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

pub(super) fn run(
    input: OwnedFd,
    output: OwnedFd,
    mut wake_fd: UnixStream,
    wake: &Arc<Wake>,
    send: &InputSender,
    output_queue: &mpsc::Receiver<OutputRequest>,
    interrupts: &tokio::sync::watch::Sender<()>,
) -> io::Result<()> {
    let mut terminal = TerminalIo::open(input, output)?;
    let execution = pump(
        &terminal,
        &mut wake_fd,
        wake,
        send,
        output_queue,
        interrupts,
    );
    let restored = terminal.restore();
    execution.and(restored)
}

fn pump(
    terminal: &TerminalIo,
    wake_fd: &mut UnixStream,
    wake: &Arc<Wake>,
    send: &InputSender,
    output_queue: &mpsc::Receiver<OutputRequest>,
    interrupts: &tokio::sync::watch::Sender<()>,
) -> io::Result<()> {
    let mut lines = Lines::new(terminal.interactive());
    let mut echo = Vec::with_capacity(MAX_ECHO_BYTES);
    if terminal.interactive() {
        echo.extend_from_slice(b"rw> ");
    }
    let mut echo_offset = 0;
    let mut output: Option<OutputRequest> = None;
    let mut input_buffer = [0_u8; 4096];
    let mut stalled_since = Instant::now();
    loop {
        if wake.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if output.is_none() && echo.is_empty() {
            output = output_queue.try_recv().ok();
        }
        if output
            .as_ref()
            .is_some_and(|pending| pending.message.is_empty())
        {
            finish(&mut output);
        }
        let writing = output.is_some() || echo_offset < echo.len();
        if !writing {
            stalled_since = Instant::now();
        }
        if writing && stalled_since.elapsed() >= Duration::from_secs(5) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "REPL output remained blocked; unsubmitted input was refused",
            ));
        }
        let ready = readiness(terminal, wake_fd, lines.ended, writing)?;
        if ready[0].intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            let mut bytes = [0_u8; 64];
            while wake_fd.read(&mut bytes).is_ok_and(|count| count != 0) {}
            if wake.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        if ready[2].intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "REPL output closed",
            ));
        }
        if ready[2].contains(PollFlags::POLLOUT) {
            if let Some(pending) = &mut output {
                let count = write_some(
                    &terminal.output,
                    &pending.message.as_bytes()[pending.offset..],
                )?;
                pending.offset += count;
                if count > 0 {
                    stalled_since = Instant::now();
                }
                if pending.offset == pending.message.len() {
                    finish(&mut output);
                }
            } else if echo_offset < echo.len() {
                let count = write_some(&terminal.output, &echo[echo_offset..])?;
                echo_offset += count;
                if count > 0 {
                    stalled_since = Instant::now();
                }
                if echo_offset == echo.len() {
                    echo.clear();
                    echo_offset = 0;
                }
            }
        }
        if !lines.ended && ready[1].intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            match rustix::io::read(&terminal.input, &mut input_buffer) {
                Ok(0) => lines.eof(send)?,
                Ok(count) => lines.push(&input_buffer[..count], send, &mut echo, interrupts)?,
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if ready[1].intersects(PollFlags::POLLERR | PollFlags::POLLNVAL) {
            return Err(io::Error::other("REPL input closed unexpectedly"));
        }
    }
}

fn write_some(output: &OwnedFd, bytes: &[u8]) -> io::Result<usize> {
    match rustix::io::write(output, &bytes[..bytes.len().min(4096)]) {
        Ok(0) => Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "REPL output made no progress",
        )),
        Ok(count) => Ok(count),
        Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn finish(output: &mut Option<OutputRequest>) {
    if let Some(OutputRequest {
        message,
        done,
        _slot: slot,
        ..
    }) = output.take()
    {
        drop(message);
        drop(slot);
        let _ = done.send(Ok(()));
    }
}

fn readiness(
    terminal: &TerminalIo,
    wake_fd: &UnixStream,
    ended: bool,
    writing: bool,
) -> io::Result<[PollFlags; 3]> {
    let mut pollers = [
        PollFd::new(wake_fd.as_fd(), PollFlags::POLLIN),
        PollFd::new(
            if ended {
                wake_fd.as_fd()
            } else {
                terminal.input.as_fd()
            },
            if ended {
                PollFlags::empty()
            } else {
                PollFlags::POLLIN
            },
        ),
        PollFd::new(
            terminal.output.as_fd(),
            if writing {
                PollFlags::POLLOUT
            } else {
                PollFlags::empty()
            },
        ),
    ];
    match poll(&mut pollers, PollTimeout::from(100_u16)) {
        Ok(_) => {}
        Err(nix::errno::Errno::EINTR) => return Ok([PollFlags::empty(); 3]),
        Err(error) => return Err(io::Error::other(error)),
    }
    Ok(pollers.map(|poller| poller.revents().unwrap_or_else(PollFlags::empty)))
}
