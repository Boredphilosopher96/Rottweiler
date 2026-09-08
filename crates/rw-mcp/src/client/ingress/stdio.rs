//! Cancellation-safe framing keeps physical decode work in the transport owner.
use super::{
    Ingress,
    frame::RawFrame,
    message::{Delivery, DeliveryRetention},
};
use crate::{McpError, payload_work::Allocation};
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    service::RoleClient,
    transport::Transport,
};
use rw_tools::CancellationToken;
use std::{io, pin::Pin, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    sync::Mutex,
    task::JoinHandle,
};

type Reader = Pin<Box<dyn AsyncRead + Send>>;
type Writer = Pin<Box<dyn AsyncWrite + Send>>;
type PendingDecode = JoinHandle<Result<super::message::InboundPacket, McpError>>;

pub(in crate::client) struct StdioTransport {
    reader: Reader,
    writer: Arc<Mutex<Writer>>,
    ingress: Arc<Ingress>,
    frame: Option<RawFrame>,
    pending: Option<PendingDecode>,
    control: Option<JoinHandle<io::Result<()>>>,
    last_delivery: Option<Arc<DeliveryRetention>>,
    input: [u8; 8192],
    start: usize,
    end: usize,
    eof: bool,
    stopped: CancellationToken,
    _scratch: Allocation,
}

impl StdioTransport {
    pub(in crate::client) fn new(
        reader: Reader,
        writer: Writer,
        ingress: Arc<Ingress>,
    ) -> Result<Self, McpError> {
        Ok(Self {
            reader,
            writer: Arc::new(Mutex::new(writer)),
            ingress,
            frame: None,
            pending: None,
            control: None,
            last_delivery: None,
            input: [0; 8192],
            start: 0,
            end: 0,
            eof: false,
            stopped: CancellationToken::default(),
            _scratch: Allocation::new(8192)?,
        })
    }

    async fn next(&mut self) -> Result<Option<ServerJsonRpcMessage>, McpError> {
        // The preceding call returned synchronously into rmcp's routing loop.
        // Request and handler owners keep their independent body leases.
        self.last_delivery = None;
        loop {
            if let Some(control) = &mut self.control {
                control
                    .await
                    .map_err(|_| super::protocol_error())?
                    .map_err(|_| super::protocol_error())?;
                self.control = None;
            }
            if let Some(pending) = &mut self.pending {
                let result = pending.await.map_err(|_| super::protocol_error())?;
                self.pending = None;
                match result?.dispatch() {
                    Delivery::Response(message, retained) => {
                        self.last_delivery = Some(retained);
                        return Ok(Some(message));
                    }
                    Delivery::Reply(reply, retained) => {
                        let send = self.send(reply);
                        self.control = Some(super::message::control(send, retained));
                    }
                    Delivery::Consumed => {}
                }
                continue;
            }
            if self.eof && self.start == self.end {
                return Ok(None);
            }
            if self.frame.is_none() {
                self.frame = Some(RawFrame::new(super::frame::STDIO_FRAME_BYTES)?);
            }
            if self.start == self.end {
                self.end = self
                    .reader
                    .read(&mut self.input)
                    .await
                    .map_err(|_| super::protocol_error())?;
                self.start = 0;
                self.eof = self.end == 0;
            }
            let remaining = &self.input[self.start..self.end];
            let newline = remaining.iter().position(|byte| *byte == b'\n');
            let length = newline.unwrap_or(remaining.len());
            self.frame
                .as_mut()
                .ok_or_else(super::protocol_error)?
                .append(&remaining[..length])?;
            self.start += length + usize::from(newline.is_some());
            if newline.is_some() || self.eof {
                let frame = self.frame.take().ok_or_else(super::protocol_error)?;
                if frame.bytes.is_empty() {
                    continue;
                }
                let ingress = Arc::clone(&self.ingress);
                // Store the handle before any await. A cancelled receive resumes
                // this exact worker instead of losing its frame or decoded result.
                let work = DecodeWork {
                    frame,
                    ingress,
                    job: self.ingress.jobs.retain()?,
                };
                self.pending = Some(tokio::spawn(work.run()));
            }
        }
    }
}

impl Transport<RoleClient> for StdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        mut message: ClientJsonRpcMessage,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let admission = self.ingress.bind_outbound(&mut message);
        let writer = Arc::clone(&self.writer);
        let stopped = self.stopped.clone();
        let jobs = Arc::clone(&self.ingress.jobs);
        let job = jobs.retain();
        async move {
            let _job = job.map_err(io_failure)?;
            admission.map_err(io_failure)?;
            if stopped.is_cancelled() {
                return Err(closed());
            }
            let mut guard = SendGuard {
                stopped: stopped.clone(),
                complete: false,
            };
            let encoded = jobs
                .run(
                    rw_resources::ResourceClass::Cpu,
                    stopped.clone(),
                    move |_| encode(&message),
                )
                .await
                .map_err(io_failure)??;
            let mut writer = tokio::select! {
                writer = writer.lock() => writer,
                () = stopped.cancelled() => return Err(closed()),
            };
            tokio::select! {
                result = writer.write_all(&encoded.bytes) => result?,
                () = stopped.cancelled() => return Err(closed()),
            }
            drop(encoded);
            guard.complete = true;
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
        let stopped = self.stopped.clone();
        let result = tokio::select! {
            result = self.next() => result,
            () = stopped.cancelled() => return None,
        };
        if let Ok(message) = result {
            message
        } else {
            self.stopped.cancel();
            None
        }
    }

    async fn close(&mut self) -> io::Result<()> {
        self.stopped.cancel();
        self.ingress.close();
        if let Some(pending) = &mut self.pending {
            // Even failed callers leave the actual decode worker and its bytes
            // owned until this join proves retirement.
            let _ = pending.await;
        }
        self.pending = None;
        if let Some(control) = &mut self.control {
            let _ = control.await;
        }
        self.control = None;
        self.ingress.jobs.settle().await;
        self.last_delivery = None;
        self.frame = None;
        self.writer.lock().await.shutdown().await
    }
}

struct SendGuard {
    stopped: CancellationToken,
    complete: bool,
}
impl Drop for SendGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.stopped.cancel();
        }
    }
}

struct Encoded {
    bytes: Vec<u8>,
    _retained: Allocation,
}

fn encode(message: &ClientJsonRpcMessage) -> io::Result<Encoded> {
    let limit = super::frame::STDIO_FRAME_BYTES;
    let mut count = rw_types::json_encoding::JsonWriter::count(limit - 1);
    count.serialize(&message).map_err(io::Error::other)?;
    let capacity = count.written() + 1;
    let retained = Allocation::new(capacity).map_err(io_failure)?;
    let mut bytes = Vec::with_capacity(capacity);
    rw_types::json_encoding::JsonWriter::buffer(&mut bytes, capacity, 0)
        .and_then(|mut output| output.serialize(&message).map_err(io::Error::other))?;
    bytes.push(b'\n');
    Ok(Encoded {
        bytes,
        _retained: retained,
    })
}
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "MCP transport is closed")
}
fn io_failure(_: impl std::fmt::Display) -> io::Error {
    io::Error::other("MCP transport admission or I/O failed")
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.stopped.cancel();
        self.ingress.close();
    }
}

#[cfg(test)]
mod tests;

struct DecodeWork {
    frame: RawFrame,
    ingress: Arc<Ingress>,
    job: crate::payload_work::Job,
}
impl DecodeWork {
    async fn run(self) -> Result<super::message::InboundPacket, McpError> {
        let result = self.ingress.decode(self.frame).await;
        drop(self.job);
        result
    }
}
