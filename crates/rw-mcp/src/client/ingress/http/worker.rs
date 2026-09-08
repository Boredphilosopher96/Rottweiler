//! One bounded private runtime owns all physical HTTP work for a connection.
use super::state::invalid;
use crate::{
    McpError, McpHttpBody, McpHttpClient, McpHttpMethod,
    payload_work::{Allocation, Job, Jobs},
};
use futures_util::StreamExt as _;
use rw_tools::CancellationToken;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};

const THREAD_STACK: usize = 2 * 1024 * 1024;
const RUNTIME_BYTES: usize = 2 * THREAD_STACK + 256 * 1024;
const CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXCHANGES: usize = 64;
fn runtimes() -> &'static Arc<Semaphore> {
    static RUNTIMES: OnceLock<Arc<Semaphore>> = OnceLock::new();
    RUNTIMES.get_or_init(|| Arc::new(Semaphore::new(64)))
}

pub(super) struct Chunk {
    pub(super) bytes: Vec<u8>,
    _retained: Arc<Allocation>,
}
pub(super) struct Response {
    pub(super) status: u16,
    pub(super) headers: Vec<(String, String)>,
    pub(super) chunks: mpsc::Receiver<Result<Chunk, McpError>>,
    discard: Arc<AtomicBool>,
    stopped: CancellationToken,
    armed: bool,
    _headers: Arc<ExchangeRetention>,
}
impl Response {
    /// An RPC response can end a POST stream before the server closes HTTP.
    /// Transfer the remaining bounded drain to its physical worker.
    pub(super) fn discard(mut self) {
        self.discard.store(true, Ordering::Release);
        self.armed = false;
    }
    pub(super) fn complete(&mut self) {
        self.armed = false;
    }
}
impl Drop for Response {
    fn drop(&mut self) {
        if self.armed {
            self.stopped.cancel();
        }
    }
}
struct ExchangeRetention {
    _input: Arc<Allocation>,
    network: Arc<Allocation>,
}
struct Command {
    method: McpHttpMethod,
    headers: Vec<(String, String)>,
    body: McpHttpBody,
    response: oneshot::Sender<Result<Response, McpError>>,
    retained: Arc<ExchangeRetention>,
}
pub(super) struct RuntimeHost {
    sender: mpsc::Sender<Command>,
    stopped: CancellationToken,
}
impl RuntimeHost {
    pub(super) fn new(
        client: Arc<dyn McpHttpClient>,
        endpoint: String,
        stopped: CancellationToken,
        jobs: &Arc<Jobs>,
    ) -> Result<Self, McpError> {
        let count = Arc::clone(runtimes())
            .try_acquire_owned()
            .map_err(|_| invalid())?;
        let stack = Allocation::new(RUNTIME_BYTES)?;
        let job = jobs.retain()?;
        let (sender, receiver) = mpsc::channel(MAX_EXCHANGES);
        let owner = RuntimeWork {
            client,
            endpoint: endpoint.into(),
            stopped: stopped.clone(),
            receiver,
        };
        let (complete, completed) = oneshot::channel::<()>();
        let worker = std::thread::Builder::new()
            .name("rw-mcp-http".into())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let _completion = complete;
                owner.run();
            })
            .map_err(|_| invalid())?;
        // No finite Blocking permit surrounds this persistent runtime: DNS uses
        // that shared pool itself. The bounded worker and its one DNS thread
        // have explicit stack admission, retained through the actual join.
        let retirement = Retirement {
            worker: Some(worker),
            joining: None,
            completed,
            credit: Some(Arc::new(RuntimeCredit {
                _stack: stack,
                _count: count,
                _job: job,
            })),
        };
        tokio::spawn(retirement.run());
        Ok(Self { sender, stopped })
    }
    pub(super) async fn request(
        &self,
        method: McpHttpMethod,
        headers: Vec<(String, String)>,
        body: McpHttpBody,
        retained: Arc<Allocation>,
    ) -> Result<Response, McpError> {
        let (response, wait) = oneshot::channel();
        let retained = Arc::new(ExchangeRetention {
            _input: retained,
            network: Arc::new(Allocation::new(4 * CHUNK_BYTES)?),
        });
        let command = Command {
            method,
            headers,
            body,
            response,
            retained,
        };
        let mut caller = Caller {
            stopped: self.stopped.clone(),
            complete: false,
        };
        tokio::select! { result = self.sender.send(command) => result.map_err(|_| invalid())?, () = self.stopped.cancelled() => return Err(invalid()) }
        let response = tokio::select! { result = wait => result.map_err(|_| invalid())?, () = self.stopped.cancelled() => return Err(invalid()) };
        caller.complete = true;
        response
    }
}
impl Drop for RuntimeHost {
    fn drop(&mut self) {
        self.stopped.cancel();
    }
}
struct Caller {
    stopped: CancellationToken,
    complete: bool,
}
impl Drop for Caller {
    fn drop(&mut self) {
        if !self.complete {
            self.stopped.cancel();
        }
    }
}
struct Retirement {
    worker: Option<std::thread::JoinHandle<()>>,
    joining: Option<JoinHandle<std::thread::Result<()>>>,
    completed: oneshot::Receiver<()>,
    credit: Option<Arc<RuntimeCredit>>,
}
impl Retirement {
    async fn run(mut self) {
        // Sender destruction follows runtime teardown on success and panic.
        // This wakeup only schedules the final join; it is not stack retirement.
        let _ = (&mut self.completed).await;
        if let Some(worker) = self.worker.take() {
            // Cleanup joins are exempt from finite work admission: a DNS worker
            // may need that admission to finish. Store the join before awaiting.
            let work = Joining {
                worker: Some(worker),
                credit: self.credit.clone(),
                complete: false,
            };
            self.joining = Some(tokio::task::spawn_blocking(move || work.run()));
        }
        if let Some(joining) = &mut self.joining
            && joining.await.is_err()
        {
            return;
        }
        // Ok(Err(panic)) is still a completed native join. An outer JoinError
        // is not; return above leaves every remaining owner quarantined by Drop.
        self.joining = None;
        self.credit = None;
    }
}
impl Drop for Retirement {
    fn drop(&mut self) {
        if self.worker.is_some() || self.joining.is_some() {
            // Runtime shutdown can abandon the async join, never its proof or
            // stack credit. Keep the physical handle and exact remaining owner.
            let owner = (self.worker.take(), self.joining.take(), self.credit.take());
            let _ = Box::leak(Box::new(owner));
        }
    }
}
struct RuntimeCredit {
    _stack: Allocation,
    _count: OwnedSemaphorePermit,
    _job: Job,
}
struct Joining {
    worker: Option<std::thread::JoinHandle<()>>,
    credit: Option<Arc<RuntimeCredit>>,
    complete: bool,
}
impl Joining {
    fn run(mut self) -> std::thread::Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let result = worker.join();
        self.complete = true;
        result
    }
}
impl Drop for Joining {
    fn drop(&mut self) {
        if !self.complete {
            // A queued blocking closure can be cancelled before it runs. Its
            // destructor must preserve the actual native handle and admission.
            let owner = (self.worker.take(), self.credit.take());
            let _ = Box::leak(Box::new(owner));
        }
    }
}
struct RuntimeScope {
    runtime: tokio::runtime::Runtime,
    retained: BTreeMap<u64, Arc<ExchangeRetention>>,
}
struct RuntimeWork {
    client: Arc<dyn McpHttpClient>,
    endpoint: Arc<str>,
    stopped: CancellationToken,
    receiver: mpsc::Receiver<Command>,
}
impl RuntimeWork {
    fn run(mut self) {
        let _exit = Caller {
            stopped: self.stopped.clone(),
            complete: false,
        };
        loop {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .thread_stack_size(THREAD_STACK)
                .build()
            else {
                return;
            };
            let mut scope = RuntimeScope {
                runtime,
                retained: BTreeMap::new(),
            };
            let restart = scope.runtime.block_on(self.serve(&mut scope.retained));
            // Both normal and unwinding destruction retire runtime first. A GET
            // network failure drains this epoch before accepting queued retries.
            drop(scope);
            if !restart {
                return;
            }
        }
    }
    async fn serve(&mut self, retained: &mut BTreeMap<u64, Arc<ExchangeRetention>>) -> bool {
        let mut tasks = JoinSet::new();
        let mut next = 0_u64;
        let mut draining = false;
        loop {
            if draining && tasks.is_empty() {
                return true;
            }
            tokio::select! {
                () = self.stopped.cancelled() => break,
                result = tasks.join_next(), if !tasks.is_empty() => {
                    match result {
                        Some(Ok((id, _, Ok(())))) => { retained.remove(&id); }
                        Some(Ok((_, McpHttpMethod::Get, Err(McpError::Transport)))) => { draining = true; }
                        _ => { self.stopped.cancel(); break; }
                    }
                }
                command = self.receiver.recv(), if !draining && tasks.len() < MAX_EXCHANGES => {
                    let Some(command) = command else { break; };
                    let Some(id) = next.checked_add(1) else { self.stopped.cancel(); break; };
                    next = id;
                    retained.insert(id, Arc::clone(&command.retained));
                    let method = command.method;
                    let work = Exchange { command, client: Arc::clone(&self.client), endpoint: Arc::clone(&self.endpoint), stopped: self.stopped.clone() };
                    tasks.spawn(async move { (id, method, work.run().await) });
                }
            }
        }
        self.receiver.close();
        while self.receiver.try_recv().is_ok() {}
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        false
    }
}
struct Exchange {
    command: Command,
    client: Arc<dyn McpHttpClient>,
    endpoint: Arc<str>,
    stopped: CancellationToken,
}
impl Exchange {
    async fn run(self) -> Result<(), McpError> {
        let Command {
            method,
            headers,
            body,
            response,
            retained,
        } = self.command;
        let network = Arc::clone(&retained.network);
        let Ok(mut incoming) = self
            .client
            .request(method, &self.endpoint, headers, body)
            .await
        else {
            let _ = response.send(Err(McpError::Transport));
            return Err(McpError::Transport);
        };
        crate::http_io::validate_response_headers(&incoming.headers)?;
        let (sender, chunks) = mpsc::channel(1);
        let discard = Arc::new(AtomicBool::new(false));
        let head = Response {
            status: incoming.status,
            headers: incoming.headers,
            chunks,
            discard: Arc::clone(&discard),
            stopped: self.stopped.clone(),
            armed: true,
            _headers: retained,
        };
        if response.send(Ok(head)).is_err() {
            return Err(invalid());
        }
        loop {
            let chunk = incoming.body.next().await;
            let Some(chunk) = chunk else {
                return Ok(());
            };
            let Ok(bytes) = chunk else {
                let _ = sender.send(Err(McpError::Transport)).await;
                return Err(McpError::Transport);
            };
            if bytes.len() > CHUNK_BYTES || bytes.capacity() > CHUNK_BYTES {
                return Err(invalid());
            }
            if discard.load(Ordering::Acquire) {
                drop(bytes);
                continue;
            }
            let chunk = Chunk {
                bytes,
                _retained: Arc::clone(&network),
            };
            if sender.send(Ok(chunk)).await.is_err() && !discard.load(Ordering::Acquire) {
                return Err(invalid());
            }
        }
    }
}

#[cfg(test)]
mod tests;
