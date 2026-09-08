//! One wakeable descriptor worker owns stdio flags and acknowledges physical writes.
use super::{RottweilerMcpServer, dispatch};
use crate::payload_work::Allocation;
use nix::sys::select::{FD_SETSIZE, FdSet, select};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rw_tools::CancellationToken;
use std::{
    io::{self, Read as _, Write as _},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{Semaphore, oneshot},
};

#[cfg(test)]
mod tests;

const CHUNK: usize = 16 * 1024;
const STACK: usize = 2 * 1024 * 1024;
struct Wake {
    socket: UnixStream,
    cancelled: AtomicBool,
}
impl Wake {
    fn notify(&self) {
        let _ = (&self.socket).write(&[1]);
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify();
    }
}
struct Cancel(CancellationToken);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
struct Descriptor {
    fd: OwnedFd,
    flags: OFlags,
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        let _ = fcntl_setfl(&self.fd, self.flags);
    }
}

enum Command {
    Read(oneshot::Sender<io::Result<Vec<u8>>>),
    Write {
        bytes: Vec<u8>,
        done: oneshot::Sender<io::Result<usize>>,
    },
}
struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
    done: oneshot::Sender<io::Result<usize>>,
}
struct Worker {
    input: Descriptor,
    output: Descriptor,
    wake_socket: UnixStream,
    wake: Arc<Wake>,
    commands: mpsc::Receiver<Command>,
    read: Option<oneshot::Sender<io::Result<Vec<u8>>>>,
    write: Option<PendingWrite>,
    _allocation: Arc<Allocation>,
}
struct NativeStream {
    wake: Arc<Wake>,
    commands: mpsc::SyncSender<Command>,
    read: Option<oneshot::Receiver<io::Result<Vec<u8>>>>,
    write: Option<oneshot::Receiver<io::Result<usize>>>,
    buffered: Vec<u8>,
    offset: usize,
    _allocation: Arc<Allocation>,
}
impl Drop for NativeStream {
    fn drop(&mut self) {
        self.wake.cancel();
    }
}
struct Physical {
    thread: Option<std::thread::JoinHandle<()>>,
    wake: Arc<Wake>,
    _stack: Allocation,
    _stdio: tokio::sync::OwnedSemaphorePermit,
}
impl Physical {
    fn join(mut self) -> io::Result<()> {
        self.wake.cancel();
        self.thread
            .take()
            .ok_or_else(|| io::Error::other("MCP native thread join missing"))?
            .join()
            .map_err(|_| io::Error::other("MCP native thread panicked"))
    }
}
impl Drop for Physical {
    fn drop(&mut self) {
        // Unpolled cleanup and runtime loss still signal and join the nonblocking
        // descriptor worker before its stack and exclusive stdio permit retire.
        self.wake.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
struct NativeOwner {
    wake: Arc<Wake>,
    finished: oneshot::Receiver<io::Result<()>>,
    physical: Option<Physical>,
}
impl Drop for NativeOwner {
    fn drop(&mut self) {
        self.wake.cancel();
        if let Some(physical) = self.physical.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || physical.join());
            } else {
                drop(physical);
            }
        }
    }
}
impl NativeOwner {
    async fn close(mut self) -> io::Result<()> {
        self.wake.cancel();
        let result = (&mut self.finished)
            .await
            .map_err(|_| io::Error::other("MCP stdio worker failed"));
        let physical = self
            .physical
            .take()
            .ok_or_else(|| io::Error::other("MCP native owner missing"))?;
        let joined = tokio::task::spawn_blocking(move || physical.join())
            .await
            .map_err(|_| io::Error::other("MCP native join failed"))?;
        result?.and(joined)
    }
}

pub(super) async fn serve(server: RottweilerMcpServer) -> io::Result<()> {
    static STDIO: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = Arc::clone(STDIO.get_or_init(|| Arc::new(Semaphore::new(1))))
        .try_acquire_owned()
        .map_err(|_| io::Error::other("MCP stdio already has an owner"))?;
    let stop = CancellationToken::default();
    let caller = Cancel(stop.clone());
    let result = tokio::spawn(async move {
        let (stream, owner) = open(
            rustix::io::dup(std::io::stdin())?,
            rustix::io::dup(std::io::stdout())?,
            permit,
        )?;
        let result = dispatch::run(server, stream, stop).await;
        let settled = owner.close().await;
        result.and(settled)
    })
    .await
    .map_err(|_| io::Error::other("MCP stdio connection failed"))?;
    drop(caller);
    result
}
fn open(
    input: OwnedFd,
    output: OwnedFd,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> io::Result<(NativeStream, NativeOwner)> {
    // Both original flags are captured before either descriptor is changed;
    // redirection may make stdin/stdout share the same open-file description.
    let input_flags = fcntl_getfl(&input)?;
    let output_flags = fcntl_getfl(&output)?;
    let input = Descriptor {
        fd: input,
        flags: input_flags,
    };
    let output = Descriptor {
        fd: output,
        flags: output_flags,
    };
    let (wake_socket, signal) = UnixStream::pair()?;
    for fd in [
        input.fd.as_raw_fd(),
        output.fd.as_raw_fd(),
        wake_socket.as_raw_fd(),
    ] {
        if usize::try_from(fd).map_or(true, |fd| fd >= FD_SETSIZE) {
            return Err(io::Error::other(
                "MCP stdio descriptor exceeds select capacity",
            ));
        }
    }
    signal.set_nonblocking(true)?;
    wake_socket.set_nonblocking(true)?;
    fcntl_setfl(&input.fd, input_flags | OFlags::NONBLOCK)?;
    fcntl_setfl(&output.fd, output_flags | OFlags::NONBLOCK)?;
    // Two queued commands, one active write, one read reply, stream remainder,
    // and read/wake scratch fit this fixed owner. No per-line native queue.
    let allocation = Arc::new(Allocation::new(CHUNK * 8).map_err(io::Error::other)?);
    let wake = Arc::new(Wake {
        socket: signal,
        cancelled: AtomicBool::new(false),
    });
    let (commands, receive) = mpsc::sync_channel(2);
    let (finished, result) = oneshot::channel();
    let worker = Worker {
        input,
        output,
        wake_socket,
        wake: Arc::clone(&wake),
        commands: receive,
        read: None,
        write: None,
        _allocation: Arc::clone(&allocation),
    };
    let stack = Allocation::new(STACK).map_err(io::Error::other)?;
    let thread = std::thread::Builder::new()
        .stack_size(STACK)
        .name("mcp-stdio".to_owned())
        .spawn(move || {
            let outcome = worker.run();
            let _ = finished.send(outcome);
        })?;
    Ok((
        NativeStream {
            wake: Arc::clone(&wake),
            commands,
            read: None,
            write: None,
            buffered: Vec::new(),
            offset: 0,
            _allocation: allocation,
        },
        NativeOwner {
            wake: Arc::clone(&wake),
            finished: result,
            physical: Some(Physical {
                thread: Some(thread),
                wake: Arc::clone(&wake),
                _stack: stack,
                _stdio: permit,
            }),
        },
    ))
}
impl Worker {
    fn run(mut self) -> io::Result<()> {
        let result = self.pump();
        // Report flag restoration errors before acknowledging physical retirement.
        let input = fcntl_setfl(&self.input.fd, self.input.flags).map_err(io::Error::from);
        let output = fcntl_setfl(&self.output.fd, self.output.flags).map_err(io::Error::from);
        result.and(input).and(output)
    }
    fn pump(&mut self) -> io::Result<()> {
        loop {
            if self.wake.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            while let Ok(command) = self.commands.try_recv() {
                match command {
                    Command::Read(done) => {
                        if self.read.replace(done).is_some() {
                            return Err(io::Error::other("MCP duplicate native read"));
                        }
                    }
                    Command::Write { bytes, done } => {
                        if self
                            .write
                            .replace(PendingWrite {
                                bytes,
                                offset: 0,
                                done,
                            })
                            .is_some()
                        {
                            return Err(io::Error::other("MCP duplicate native write"));
                        }
                    }
                }
            }
            let mut reads = FdSet::new();
            let mut writes = FdSet::new();
            reads.insert(self.wake_socket.as_fd());
            if self.read.is_some() {
                reads.insert(self.input.fd.as_fd());
            }
            if self.write.is_some() {
                writes.insert(self.output.fd.as_fd());
            }
            match select(None, Some(&mut reads), Some(&mut writes), None, None) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            }
            let input_ready = reads.contains(self.input.fd.as_fd());
            let output_ready = writes.contains(self.output.fd.as_fd());
            if reads.contains(self.wake_socket.as_fd()) {
                let mut scratch = [0_u8; 64];
                let _ = self.wake_socket.read(&mut scratch);
            }
            if self.wake.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            if input_ready {
                self.read_ready()?;
            }
            if output_ready {
                self.write_ready()?;
            }
        }
    }
    fn read_ready(&mut self) -> io::Result<()> {
        let mut bytes = vec![0; CHUNK];
        match rustix::io::read(&self.input.fd, &mut bytes) {
            Ok(length) => {
                bytes.truncate(length);
                if let Some(done) = self.read.take() {
                    let _ = done.send(Ok(bytes));
                }
                Ok(())
            }
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    fn write_ready(&mut self) -> io::Result<()> {
        let Some(pending) = self.write.as_mut() else {
            return Ok(());
        };
        match rustix::io::write(&self.output.fd, &pending.bytes[pending.offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "MCP stdout closed",
                ));
            }
            Ok(length) => pending.offset += length,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        if pending.offset == pending.bytes.len()
            && let Some(pending) = self.write.take()
        {
            let _ = pending.done.send(Ok(pending.offset));
        }
        Ok(())
    }
}
impl AsyncRead for NativeStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.offset < this.buffered.len() {
            let length = output.remaining().min(this.buffered.len() - this.offset);
            output.put_slice(&this.buffered[this.offset..this.offset + length]);
            this.offset += length;
            return Poll::Ready(Ok(()));
        }
        if this.read.is_none() {
            this.buffered.clear();
            this.offset = 0;
            let (done, wait) = oneshot::channel();
            this.commands
                .try_send(Command::Read(done))
                .map_err(|_| io::Error::other("MCP native read admission failed"))?;
            this.read = Some(wait);
            this.wake.notify();
        }
        let Some(wait) = this.read.as_mut() else {
            return Poll::Ready(Err(io::Error::other("MCP native read missing")));
        };
        match std::future::Future::poll(Pin::new(wait), cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.read = None;
                this.buffered =
                    result.map_err(|_| io::Error::other("MCP native read stopped"))??;
                let length = output.remaining().min(this.buffered.len());
                output.put_slice(&this.buffered[..length]);
                this.offset = length;
                Poll::Ready(Ok(()))
            }
        }
    }
}
impl AsyncWrite for NativeStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.write.is_none() {
            if bytes.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let bytes = bytes[..bytes.len().min(CHUNK)].to_vec();
            let (done, wait) = oneshot::channel();
            this.commands
                .try_send(Command::Write { bytes, done })
                .map_err(|_| io::Error::other("MCP native write admission failed"))?;
            this.write = Some(wait);
            this.wake.notify();
        }
        let Some(wait) = this.write.as_mut() else {
            return Poll::Ready(Err(io::Error::other("MCP native write missing")));
        };
        match std::future::Future::poll(Pin::new(wait), cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.write = None;
                Poll::Ready(result.map_err(|_| io::Error::other("MCP native write stopped"))?)
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
