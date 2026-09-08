#![allow(clippy::expect_used)]
use super::*;
use async_trait::async_trait;
use rmcp::{
    ServiceExt as _,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use std::{io, sync::atomic::AtomicUsize};
use tokio::sync::{Notify, Semaphore, mpsc};

struct Completion {
    entered: Notify,
    release: Semaphore,
    finished: AtomicUsize,
    dropped: AtomicUsize,
}
impl Default for Completion {
    fn default() -> Self {
        Self {
            entered: Notify::new(),
            release: Semaphore::new(0),
            finished: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }
    }
}
impl Completion {
    async fn finish(&self) {
        self.entered.notify_one();
        self.release
            .acquire()
            .await
            .expect("cleanup release")
            .forget();
        self.finished.fetch_add(1, Ordering::SeqCst);
    }
}

struct HeldTransport {
    sender: mpsc::Sender<RxJsonRpcMessage<RoleClient>>,
    receiver: mpsc::Receiver<RxJsonRpcMessage<RoleClient>>,
    completion: Arc<Completion>,
}
impl Transport<RoleClient> for HeldTransport {
    type Error = io::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let sender = self.sender.clone();
        async move {
            let value = serde_json::to_value(item).expect("fixture request");
            if value["method"] == "initialize" {
                let response = serde_json::json!({"jsonrpc":"2.0", "id":value["id"], "result": {
                    "protocolVersion":rmcp::model::ProtocolVersion::default(),
                    "capabilities":{}, "serverInfo":{"name":"owned-close", "version":"1"}
                }});
                sender
                    .send(serde_json::from_value(response).expect("initialize response"))
                    .await
                    .map_err(|_| io::Error::other("fixture receiver closed"))?;
            }
            Ok(())
        }
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.receiver.recv().await
    }
    async fn close(&mut self) -> io::Result<()> {
        self.completion.finish().await;
        Ok(())
    }
}
impl Drop for HeldTransport {
    fn drop(&mut self) {
        self.completion.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct HeldProcess(Arc<Completion>);
#[async_trait]
impl ProtocolProcessHandle for HeldProcess {
    async fn observe_exit(&mut self, _: Duration) -> io::Result<Option<std::process::ExitStatus>> {
        Ok(None)
    }
    async fn terminate_and_reap(&mut self, _: Duration) -> io::Result<()> {
        self.0.finish().await;
        Ok(())
    }
}
impl Drop for HeldProcess {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

async fn fixture() -> (
    Arc<ConnectionClosure>,
    Arc<Completion>,
    Arc<Completion>,
    Arc<super::super::ingress::Ingress>,
) {
    let transport = Arc::new(Completion::default());
    let child = Arc::new(Completion::default());
    let (sender, receiver) = mpsc::channel(4);
    let service = crate::McpInboundRouter::default()
        .serve(HeldTransport {
            sender,
            receiver,
            completion: transport.clone(),
        })
        .await
        .expect("real rmcp initialize");
    let ingress = super::super::ingress::Ingress::new(service.service().clone()).expect("ingress");
    let closure = ConnectionClosure::new(
        service,
        Some(Box::new(HeldProcess(child.clone()))),
        Arc::clone(&ingress),
    );
    (Arc::new(closure), transport, child, ingress)
}

#[tokio::test]
async fn aborted_close_waiter_retains_actual_rmcp_future_and_process_proof() {
    let (closure, transport, child, _) = fixture().await;
    let first = closure.clone();
    let waiter = tokio::spawn(async move { first.close(Duration::from_secs(3)).await });
    transport.entered.notified().await;
    child.entered.notified().await;
    waiter.abort();
    assert!(waiter.await.expect_err("cancelled waiter").is_cancelled());
    assert!(closure.is_closed());
    let next = closure.clone();
    let proof = tokio::spawn(async move { next.close(Duration::from_secs(3)).await });
    transport.release.add_permits(1);
    tokio::task::yield_now().await;
    assert!(!proof.is_finished());
    assert_eq!(child.dropped.load(Ordering::SeqCst), 0);
    child.release.add_permits(1);
    assert!(proof.await.expect("proof waiter").is_ok());
    assert_eq!(transport.finished.load(Ordering::SeqCst), 1);
    assert_eq!(child.finished.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn timed_out_proof_stays_failed_while_exact_cleanup_futures_continue() {
    let (closure, transport, child, _) = fixture().await;
    let first = closure.clone();
    let waiter = tokio::spawn(async move { first.close(Duration::from_secs(3)).await });
    transport.entered.notified().await;
    child.entered.notified().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(waiter.await.expect("deadline waiter").is_err());
    assert_eq!(transport.dropped.load(Ordering::SeqCst), 0);
    assert_eq!(child.dropped.load(Ordering::SeqCst), 0);
    transport.release.add_permits(1);
    child.release.add_permits(1);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(transport.finished.load(Ordering::SeqCst), 1);
    assert_eq!(child.finished.load(Ordering::SeqCst), 1);
    assert!(closure.close(Duration::from_secs(3)).await.is_err());
    assert_eq!(
        child.dropped.load(Ordering::SeqCst),
        1,
        "actual retirement releases the native handle while the missed deadline stays failed"
    );
}

#[tokio::test]
async fn service_shutdown_fences_admission_then_waits_for_owned_request_retirement() {
    let (closure, transport, child, ingress) = fixture().await;
    let mut request: rmcp::model::ClientRequest =
        serde_json::from_value(serde_json::json!({"method":"ping"})).expect("typed request");
    let state = ingress
        .requests
        .prepare(&mut request)
        .expect("admitted request");
    let closing = Arc::clone(&closure);
    let waiter = tokio::spawn(async move { closing.close(Duration::from_secs(3)).await });
    transport.entered.notified().await;
    child.entered.notified().await;
    assert!(ingress.requests.prepare(&mut request).is_err());
    transport.release.add_permits(1);
    child.release.add_permits(1);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(transport.finished.load(Ordering::SeqCst), 1);
    assert!(
        !waiter.is_finished(),
        "transport EOF is not request retirement"
    );
    drop(request);
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished(), "request task still owns its state");
    drop(state);
    assert!(waiter.await.expect("physical close owner").is_ok());
}
