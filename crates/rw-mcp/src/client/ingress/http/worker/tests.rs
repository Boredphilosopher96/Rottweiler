use super::*;
use crate::McpHttpResponse;
use async_trait::async_trait;
use futures_util::stream;
use std::sync::Mutex;

struct BlockingResolver {
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
#[async_trait]
impl McpHttpClient for BlockingResolver {
    async fn request(
        &self,
        _: McpHttpMethod,
        _: &str,
        _: Vec<(String, String)>,
        body: McpHttpBody,
    ) -> Result<McpHttpResponse, McpError> {
        let started = self
            .started
            .lock()
            .map_err(|_| invalid())?
            .take()
            .ok_or_else(invalid)?;
        let release = self
            .release
            .lock()
            .map_err(|_| invalid())?
            .take()
            .ok_or_else(invalid)?;
        tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(5));
            drop(body);
        });
        Ok(McpHttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Box::pin(stream::pending()),
        })
    }
}
#[tokio::test]
async fn cancelled_response_retains_body_and_runtime_until_actual_blocking_resolver_exit()
-> Result<(), Box<dyn std::error::Error>> {
    let (started, ready) = oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let client = Arc::new(BlockingResolver {
        started: Mutex::new(Some(started)),
        release: Mutex::new(Some(wait)),
    });
    let stopped = CancellationToken::default();
    let jobs = Arc::new(Jobs::default());
    let host = RuntimeHost::new(
        client,
        "http://fixture.invalid/mcp".into(),
        stopped.clone(),
        &jobs,
    )?;
    let retained = Arc::new(Allocation::new(64 * 1024)?);
    let weak = Arc::downgrade(&retained);
    let body = McpHttpBody::new(b"request".to_vec(), Arc::clone(&retained));
    let response = host
        .request(McpHttpMethod::Post, Vec::new(), body, retained)
        .await?;
    ready.await?;
    drop(response);
    assert!(stopped.is_cancelled());
    let settled = jobs.settle();
    tokio::pin!(settled);
    assert!(futures_util::poll!(settled.as_mut()).is_pending());
    assert!(
        weak.upgrade().is_some(),
        "the actual resolver still owns request bytes"
    );
    release.send(())?;
    tokio::time::timeout(std::time::Duration::from_secs(2), settled).await?;
    assert!(weak.upgrade().is_none());
    Ok(())
}

#[tokio::test]
async fn abandoned_initial_handshake_retains_ingress_until_its_resolver_and_runtime_settle()
-> Result<(), Box<dyn std::error::Error>> {
    let (started, ready) = oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let client = Arc::new(BlockingResolver {
        started: Mutex::new(Some(started)),
        release: Mutex::new(Some(wait)),
    });
    let ingress = crate::client::ingress::Ingress::new(crate::client::McpInboundRouter::default())?;
    let weak = Arc::downgrade(&ingress);
    let jobs = Arc::clone(&ingress.jobs);
    let transport = crate::client::ingress::http::HttpTransport::new(
        "http://fixture.invalid/mcp".into(),
        None,
        client,
        Arc::clone(&ingress),
        1,
    )?;
    let caller = tokio::spawn(crate::client::start::start(
        rw_types::McpServerId::new("handshake")?,
        transport,
        ingress,
        None,
    ));
    ready.await?;
    caller.abort();
    assert!(caller.await.is_err());
    assert!(
        weak.upgrade().is_some(),
        "initialization authority survives caller loss"
    );
    let settled = jobs.settle();
    tokio::pin!(settled);
    assert!(futures_util::poll!(settled.as_mut()).is_pending());
    release.send(())?;
    tokio::time::timeout(std::time::Duration::from_secs(2), settled).await?;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

struct PanickingClient;
#[async_trait]
impl McpHttpClient for PanickingClient {
    async fn request(
        &self,
        _: McpHttpMethod,
        _: &str,
        _: Vec<(String, String)>,
        _: McpHttpBody,
    ) -> Result<McpHttpResponse, McpError> {
        panic!("fixture client panic before response");
    }
}
#[tokio::test]
async fn panicked_exchange_never_acknowledges_request_and_still_joins_runtime()
-> Result<(), Box<dyn std::error::Error>> {
    let stopped = CancellationToken::default();
    let jobs = Arc::new(Jobs::default());
    let host = RuntimeHost::new(
        Arc::new(PanickingClient),
        "http://fixture.invalid/mcp".into(),
        stopped.clone(),
        &jobs,
    )?;
    let retained = Arc::new(Allocation::new(64 * 1024)?);
    let weak = Arc::downgrade(&retained);
    let body = McpHttpBody::new(Vec::new(), Arc::clone(&retained));
    assert!(
        host.request(McpHttpMethod::Post, Vec::new(), body, retained)
            .await
            .is_err()
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), jobs.settle()).await?;
    assert!(stopped.is_cancelled());
    assert!(weak.upgrade().is_none());
    Ok(())
}

#[tokio::test]
async fn cancelled_outer_join_observer_keeps_runtime_credit_unsettled()
-> Result<(), Box<dyn std::error::Error>> {
    let jobs = Arc::new(Jobs::default());
    let count = Arc::new(Semaphore::new(1)).acquire_owned().await?;
    let credit = Arc::new(RuntimeCredit {
        _stack: Allocation::new(1)?,
        _count: count,
        _job: jobs.retain()?,
    });
    let weak = Arc::downgrade(&credit);
    let (completion, completed) = oneshot::channel();
    drop(completion);
    let joining = tokio::spawn(std::future::pending::<std::thread::Result<()>>());
    joining.abort();
    let owner = Retirement {
        worker: None,
        joining: Some(joining),
        completed,
        credit: Some(credit),
    };
    owner.run().await;
    assert!(
        weak.upgrade().is_some(),
        "outer JoinError cannot refund native runtime credit"
    );
    let settled = jobs.settle();
    tokio::pin!(settled);
    assert!(futures_util::poll!(settled.as_mut()).is_pending());
    Ok(())
}

struct RecoveringGet {
    requests: std::sync::atomic::AtomicUsize,
    resolved: Arc<AtomicBool>,
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    retried: Mutex<Option<oneshot::Sender<()>>>,
}
#[async_trait]
impl McpHttpClient for RecoveringGet {
    async fn request(
        &self,
        _: McpHttpMethod,
        _: &str,
        _: Vec<(String, String)>,
        body: McpHttpBody,
    ) -> Result<McpHttpResponse, McpError> {
        if self.requests.fetch_add(1, Ordering::AcqRel) == 0 {
            let started = self
                .started
                .lock()
                .map_err(|_| invalid())?
                .take()
                .ok_or_else(invalid)?;
            let release = self
                .release
                .lock()
                .map_err(|_| invalid())?
                .take()
                .ok_or_else(invalid)?;
            let resolved = Arc::clone(&self.resolved);
            tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = release.recv_timeout(std::time::Duration::from_secs(5));
                drop(body);
                resolved.store(true, Ordering::Release);
            });
            Err(McpError::Transport)
        } else {
            if !self.resolved.load(Ordering::Acquire) {
                return Err(invalid());
            }
            let _ = self
                .retried
                .lock()
                .map_err(|_| invalid())?
                .take()
                .ok_or_else(invalid)?
                .send(());
            Ok(McpHttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::pin(stream::empty()),
            })
        }
    }
}
#[tokio::test]
async fn failed_get_epoch_settles_resolver_before_replaying_queued_transport_work()
-> Result<(), Box<dyn std::error::Error>> {
    let (started, ready) = oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let (retried, mut retry_seen) = oneshot::channel();
    let client = Arc::new(RecoveringGet {
        requests: std::sync::atomic::AtomicUsize::new(0),
        resolved: Arc::new(AtomicBool::new(false)),
        started: Mutex::new(Some(started)),
        release: Mutex::new(Some(wait)),
        retried: Mutex::new(Some(retried)),
    });
    let stopped = CancellationToken::default();
    let jobs = Arc::new(Jobs::default());
    let host = Arc::new(RuntimeHost::new(
        client,
        "http://fixture.invalid/mcp".into(),
        stopped.clone(),
        &jobs,
    )?);
    let credit = Arc::new(Allocation::new(64 * 1024)?);
    let first = McpHttpBody::new(Vec::new(), Arc::clone(&credit));
    assert!(matches!(
        host.request(McpHttpMethod::Get, Vec::new(), first, credit)
            .await,
        Err(McpError::Transport)
    ));
    ready.await?;
    assert!(!stopped.is_cancelled());
    let retry_host = Arc::clone(&host);
    let retry = tokio::spawn(async move {
        let credit = Arc::new(Allocation::new(64 * 1024)?);
        let body = McpHttpBody::new(Vec::new(), Arc::clone(&credit));
        retry_host
            .request(McpHttpMethod::Get, Vec::new(), body, credit)
            .await
    });
    tokio::task::yield_now().await;
    assert!(matches!(
        retry_seen.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    release.send(())?;
    let mut response = tokio::time::timeout(std::time::Duration::from_secs(2), retry).await???;
    retry_seen.await?;
    assert!(response.chunks.recv().await.is_none());
    response.complete();
    drop(response);
    drop(host);
    tokio::time::timeout(std::time::Duration::from_secs(2), jobs.settle()).await?;
    Ok(())
}
