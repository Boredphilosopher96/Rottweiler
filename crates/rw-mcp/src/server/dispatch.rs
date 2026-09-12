//! One connection owns request work and responses through physical retirement.
use super::{
    RottweilerMcpServer,
    lifecycle::{self, Lifecycle, Negotiated},
    wire::{self, Body},
};
use crate::{
    McpResponse,
    payload_work::{Allocation, Job, Jobs},
};
use futures_util::{StreamExt as _, stream::FuturesUnordered};
use rmcp::{
    ErrorData,
    model::{ClientRequest, RequestId, ServerJsonRpcMessage, ServerResult},
};
use rw_tools::CancellationToken;
use rw_types::json_encoding::JsonWriter;
use std::{
    collections::HashMap,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _},
    sync::mpsc,
    task::JoinHandle,
    time::Instant,
};

#[cfg(test)]
mod tests;

const REQUESTS: usize = 64;
const MAX_REPLY_BYTES: usize = crate::ingress::frame::STDIO_FRAME_BYTES;
const WRITE_DEADLINE: Duration = Duration::from_secs(30);
struct Credits {
    decoded: Arc<Allocation>,
    _job: Job,
}
struct Control {
    id: RequestId,
    cancelled: AtomicBool,
    claimed: AtomicBool,
    task_done: AtomicBool,
    reply_done: AtomicBool,
    deadline: Instant,
    credits: Credits,
}
impl Control {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if !self.claimed.swap(true, Ordering::AcqRel) {
            self.reply_done.store(true, Ordering::Release);
        }
    }
    fn retired(&self) -> bool {
        self.task_done.load(Ordering::Acquire) && self.reply_done.load(Ordering::Acquire)
    }
}
struct RequestWork {
    request: Result<ClientRequest, ErrorData>,
    negotiated: Result<Negotiated, ErrorData>,
    server: Arc<RottweilerMcpServer>,
    control: Arc<Control>,
}
struct Completed {
    reply: Option<Reply>,
    control: Arc<Control>,
}
struct Reply {
    bytes: Vec<u8>,
    control: Arc<Control>,
    _encoded: Allocation,
}
struct Encoding {
    message: ServerJsonRpcMessage,
    retained: Vec<Arc<Allocation>>,
    control: Arc<Control>,
}
impl Encoding {
    fn run(self) -> io::Result<Reply> {
        let mut count = JsonWriter::count(MAX_REPLY_BYTES - 1);
        count.serialize(&self.message).map_err(io::Error::other)?;
        let size = count.written() + 1;
        let allocation = Allocation::new(size).map_err(io::Error::other)?;
        let mut bytes = Vec::with_capacity(size);
        JsonWriter::buffer(&mut bytes, size, 0)?
            .serialize(&self.message)
            .map_err(io::Error::other)?;
        bytes.push(b'\n');
        drop(self.message);
        drop(self.retained);
        Ok(Reply {
            bytes,
            control: self.control,
            _encoded: allocation,
        })
    }
}
impl Reply {
    async fn encode(
        result: Result<McpResponse<ServerResult>, ErrorData>,
        control: Arc<Control>,
    ) -> io::Result<Self> {
        let work = match result {
            Ok(result) => Encoding {
                message: ServerJsonRpcMessage::response(result.value, control.id.clone()),
                retained: result.retained,
                control,
            },
            Err(error) => Encoding {
                message: ServerJsonRpcMessage::error(error, Some(control.id.clone())),
                retained: Vec::new(),
                control,
            },
        };
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
            .await
            .map_err(|_| io::Error::other("MCP reply encoder worker failed"))?
    }
}

impl RequestWork {
    async fn run(self) -> io::Result<Completed> {
        let result = match (self.request, self.negotiated) {
            (Ok(request), Ok(negotiated)) => {
                lifecycle::execute(
                    &self.server,
                    request,
                    negotiated,
                    Arc::clone(&self.control.credits.decoded),
                )
                .await
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
        };
        // Caller cancellation never cancels the accepted bridge operation.
        let reply = if self.control.claimed.swap(true, Ordering::AcqRel) {
            drop(result);
            None
        } else {
            Some(Reply::encode(result, Arc::clone(&self.control)).await?)
        };
        self.control.task_done.store(true, Ordering::Release);
        Ok(Completed {
            reply,
            control: self.control,
        })
    }
}
#[cfg(test)]
struct Caller(CancellationToken);
#[cfg(test)]
impl Drop for Caller {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Caller loss requests connection shutdown; the independent owner joins its
/// accepted bridge operations and writer before dropping their byte leases.
#[cfg(test)]
pub(super) async fn serve_io<T>(server: RottweilerMcpServer, io: T) -> io::Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let stop = CancellationToken::default();
    let caller = Caller(stop.clone());
    let result = tokio::spawn(run(server, io, stop))
        .await
        .map_err(|_| io::Error::other("MCP connection task failed"))?;
    drop(caller);
    result
}

pub(super) async fn run<T>(
    server: RottweilerMcpServer,
    io: T,
    stop: CancellationToken,
) -> io::Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Retained channel blocks, hash-table buckets and connection task metadata
    // remain allocated even between requests, independently of request credits.
    let metadata = Arc::new(Allocation::new(64 * 1024).map_err(io::Error::other)?);
    let server = Arc::new(server);
    let jobs = Arc::new(Jobs::default());
    let (read, write) = tokio::io::split(io);
    let mut reader = wire::Reader::new(read)?;
    let (send, receive) = mpsc::channel(REQUESTS);
    let writer_stop = CancellationToken::default();
    let mut writer = tokio::spawn(
        WriterWork {
            write,
            replies: receive,
            stop: writer_stop.clone(),
            _metadata: Arc::clone(&metadata),
        }
        .run(),
    );
    let mut tasks: FuturesUnordered<JoinHandle<io::Result<Completed>>> = FuturesUnordered::new();
    let mut pending: HashMap<RequestId, Arc<Control>> = HashMap::new();
    let mut lifecycle = Lifecycle::default();
    let mut writer_observed = false;
    let result = pump(
        &server,
        &jobs,
        &mut reader,
        &send,
        &mut writer,
        &mut tasks,
        &mut pending,
        &mut lifecycle,
        &stop,
        &mut writer_observed,
    )
    .await;
    drop(reader);
    for control in pending.values() {
        control.cancel();
    }
    writer_stop.cancel();
    drop(send);
    // JoinHandle destruction detaches, never aborts an accepted operation. On
    // the normal close path every result is consumed before returning success.
    let mut cleanup = Ok(());
    while let Some(result) = tasks.next().await {
        match result {
            Ok(Ok(completed)) => drop(completed),
            _ => cleanup = Err(io::Error::other("MCP bridge task failed during settlement")),
        }
    }
    if !writer_observed {
        match writer.await {
            Ok(result) => {
                cleanup = cleanup.and(result);
            }
            Err(_) => cleanup = Err(io::Error::other("MCP response writer failed")),
        }
    }
    drop(pending);
    jobs.settle().await;
    result.and(cleanup)
}

#[allow(clippy::too_many_arguments)]
async fn pump<R: AsyncRead + Unpin>(
    server: &Arc<RottweilerMcpServer>,
    jobs: &Arc<Jobs>,
    reader: &mut wire::Reader<R>,
    send: &mpsc::Sender<Reply>,
    writer: &mut JoinHandle<io::Result<()>>,
    tasks: &mut FuturesUnordered<JoinHandle<io::Result<Completed>>>,
    pending: &mut HashMap<RequestId, Arc<Control>>,
    lifecycle: &mut Lifecycle,
    stop: &CancellationToken,
    writer_observed: &mut bool,
) -> io::Result<()> {
    loop {
        pending.retain(|_, value| !value.retired());
        let deadline = pending
            .values()
            .filter(|value| !value.claimed.load(Ordering::Acquire))
            .map(|value| value.deadline)
            .min();
        let wake = deadline.unwrap_or_else(|| Instant::now() + WRITE_DEADLINE);
        tokio::select! {
            biased;
            () = stop.cancelled() => return Ok(()),
            result = &mut *writer => { *writer_observed = true; return result.map_err(|_| io::Error::other("MCP writer task failed"))?; },
            result = tasks.next(), if !tasks.is_empty() => {
                let completed = result.ok_or_else(|| io::Error::other("MCP request task missing"))?
                    .map_err(|_| io::Error::other("MCP request task failed"))??;
                if let Some(reply) = completed.reply { enqueue(send, reply)?; }
                drop(completed.control);
            }
            () = tokio::time::sleep_until(wake), if deadline.is_some() => {
                for control in pending.values() {
                    if control.deadline <= Instant::now() && !control.claimed.swap(true, Ordering::AcqRel) {
                        let reply = Reply::encode(Err(ErrorData::internal_error("MCP engine request timed out", None)), Arc::clone(control)).await?;
                        enqueue(send, reply)?;
                    }
                }
            }
            frame = reader.next() => {
                let Some(frame) = frame? else { return Ok(()); };
                let decoded = wire::decode(frame).await?;
                match decoded.body {
                    Body::Ignore => {}
                    Body::Cancel(id) => { if let Some(control) = pending.get(&id) { control.cancel(); } }
                    Body::Request { id, request } => {
                        let request = *request;
                        pending.retain(|_, value| !value.retired());
                        if pending.contains_key(&id) { return Err(io::Error::other("MCP duplicate active request ID")); }
                        if pending.len() >= REQUESTS { return Err(io::Error::other("MCP request admission exhausted")); }
                        let negotiated = request.as_ref().map_err(Clone::clone).and_then(|request| lifecycle.admit(request));
                        let control = Arc::new(Control {
                            id: id.clone(), cancelled: AtomicBool::new(false), claimed: AtomicBool::new(false),
                            task_done: AtomicBool::new(false), reply_done: AtomicBool::new(false),
                            deadline: Instant::now().checked_add(server.request_timeout).ok_or_else(|| io::Error::other("MCP request deadline exceeds clock range"))?,
                            credits: Credits { decoded: Arc::new(decoded.retained), _job: jobs.retain().map_err(io::Error::other)? },
                        });
                        pending.insert(id, Arc::clone(&control));
                        let work = RequestWork { request, negotiated, server: Arc::clone(server), control };
                        tasks.push(tokio::spawn(work.run()));
                    }
                }
            }
        }
    }
}
fn enqueue(send: &mpsc::Sender<Reply>, reply: Reply) -> io::Result<()> {
    send.try_send(reply)
        .map_err(|_| io::Error::other("MCP response queue admission failed"))
}
struct WriterWork<W> {
    write: W,
    replies: mpsc::Receiver<Reply>,
    stop: CancellationToken,
    _metadata: Arc<Allocation>,
}
impl<W: AsyncWrite + Unpin> WriterWork<W> {
    async fn run(mut self) -> io::Result<()> {
        loop {
            let reply = tokio::select! { biased; () = self.stop.cancelled() => break, reply = self.replies.recv() => reply };
            let Some(reply) = reply else {
                break;
            };
            if !reply.control.cancelled.load(Ordering::Acquire) {
                tokio::select! {
                    biased;
                    () = self.stop.cancelled() => break,
                    result = tokio::time::timeout(WRITE_DEADLINE, async { self.write.write_all(&reply.bytes).await?; self.write.flush().await }) => {
                        result.map_err(|_| io::Error::other("MCP stdout remained blocked"))??;
                    }
                }
            }
            reply.control.reply_done.store(true, Ordering::Release);
        }
        Ok(())
    }
}
