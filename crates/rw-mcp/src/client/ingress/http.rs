//! Raw HTTP exchanges and their message queues share the MCP allocation owner.
mod state;
mod stream;
mod wire;
mod worker;
use self::state::Shared;
use super::{
    Ingress,
    message::{Delivery, DeliveryRetention, InboundPacket},
};
use crate::{McpError, McpHttpClient, SecretToken, payload_work::Allocation};
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    service::RoleClient,
    transport::Transport,
};
use std::{io, sync::Arc};
use tokio::{sync::mpsc, task::JoinHandle};

pub(in crate::client) struct HttpTransport {
    receiver: mpsc::Receiver<InboundPacket>,
    shared: Arc<Shared>,
    get: Option<JoinHandle<Result<(), McpError>>>,
    control: Option<JoinHandle<io::Result<()>>>,
    last_delivery: Option<Arc<DeliveryRetention>>,
    get_started: bool,
    _queue: Allocation,
}
impl HttpTransport {
    pub(in crate::client) fn new(
        endpoint: String,
        token: Option<SecretToken>,
        client: Arc<dyn McpHttpClient>,
        ingress: Arc<Ingress>,
        capacity: usize,
    ) -> Result<Self, McpError> {
        if !(1..=256).contains(&capacity) {
            return Err(super::protocol_error());
        }
        let queue = Allocation::new(capacity * 1024)?;
        let (sender, receiver) = mpsc::channel(capacity);
        let shared = Shared::new(endpoint, token, client, ingress, sender)?;
        Ok(Self {
            receiver,
            shared,
            get: None,
            control: None,
            last_delivery: None,
            get_started: false,
            _queue: queue,
        })
    }
    fn start_get(&mut self) -> Result<(), McpError> {
        if !self.get_started && self.shared.should_start_get()? {
            self.get_started = true;
            let shared = Arc::clone(&self.shared);
            let job = shared.ingress.jobs.retain()?;
            self.get = Some(tokio::spawn(async move {
                let result = shared.get_stream().await;
                if result.is_err() && !shared.closing.is_cancelled() {
                    shared.stopped.cancel();
                }
                drop(job);
                result
            }));
        }
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<ServerJsonRpcMessage>, McpError> {
        self.last_delivery = None;
        loop {
            self.start_get()?;
            if let Some(control) = &mut self.control {
                control
                    .await
                    .map_err(|_| super::protocol_error())?
                    .map_err(|_| super::protocol_error())?;
                self.control = None;
            }
            let changed = self.shared.changed.notified();
            let packet = tokio::select! {
                packet = self.receiver.recv() => packet,
                () = changed => continue,
            };
            let Some(packet) = packet else {
                return Ok(None);
            };
            match packet.dispatch() {
                Delivery::Response(message, retained) => {
                    self.last_delivery = Some(retained);
                    return Ok(Some(message));
                }
                Delivery::Consumed => {}
                Delivery::Reply(reply, retained) => {
                    let send = self.send(reply);
                    self.control = Some(super::message::control(send, retained));
                }
            }
        }
    }
}
impl Transport<RoleClient> for HttpTransport {
    type Error = io::Error;
    fn send(
        &mut self,
        mut message: ClientJsonRpcMessage,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let admitted = self.shared.ingress.bind_outbound(&mut message);
        let shared = Arc::clone(&self.shared);
        let job = shared.ingress.jobs.retain();
        async move {
            let _job = job.map_err(io::Error::other)?;
            admitted.map_err(io::Error::other)?;
            let result = shared.post(message).await;
            if result.is_err() && !shared.closing.is_cancelled() {
                shared.stopped.cancel();
            }
            result.map_err(io::Error::other)
        }
    }
    async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
        let stopped = self.shared.stopped.clone();
        let result = tokio::select! {
            result = self.next() => result,
            () = stopped.cancelled() => return None,
        };
        if let Ok(message) = result {
            message
        } else {
            self.shared.stopped.cancel();
            None
        }
    }
    async fn close(&mut self) -> io::Result<()> {
        self.shared.closing.cancel();
        self.shared.ingress.close();
        self.receiver.close();
        while self.receiver.try_recv().is_ok() {}
        if let Some(get) = &mut self.get {
            let _ = get.await;
        }
        self.get = None;
        if let Some(control) = &mut self.control {
            let _ = control.await;
        }
        self.control = None;
        let deletion = if self.shared.stopped.is_cancelled() {
            Ok(())
        } else {
            self.shared.delete_session().await
        };
        self.shared.stopped.cancel();
        self.shared.ingress.jobs.settle().await;
        self.last_delivery = None;
        deletion.map_err(io::Error::other)
    }
}
impl Drop for HttpTransport {
    fn drop(&mut self) {
        self.shared.stopped.cancel();
        self.shared.ingress.close();
    }
}

#[cfg(test)]
mod tests;
