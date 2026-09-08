//! Encoded frames, correlated requests, and decoded messages share MCP admission.
pub(crate) mod decode;
mod frame;
pub(super) mod http;
mod http_headers;
mod message;
pub(super) mod requests;
mod sse;
pub(super) mod stdio;

use crate::{McpError, McpInboundRouter, payload_work::Allocation};
use frame::RawFrame;
use message::{DeliveryRetention, InboundPacket};
use requests::{RequestRegistry, RequestState};
use rmcp::model::{ClientJsonRpcMessage, GetExtensions, ServerJsonRpcMessage, ServerNotification};
use std::sync::{Arc, Mutex};

pub(super) struct Ingress {
    pub(super) requests: Arc<RequestRegistry>,
    pub(super) router: McpInboundRouter,
    bootstrap: Mutex<Vec<Arc<RequestState>>>,
    pub(super) jobs: Arc<crate::payload_work::Jobs>,
    pub(super) request_jobs: Arc<crate::payload_work::Jobs>,
}

impl Ingress {
    pub(super) fn new(router: McpInboundRouter) -> Result<Arc<Self>, McpError> {
        let request_jobs = Arc::new(crate::payload_work::Jobs::default());
        Ok(Arc::new(Self {
            requests: RequestRegistry::new(router.clone(), Arc::clone(&request_jobs))?,
            request_jobs,
            router,
            bootstrap: Mutex::new(Vec::with_capacity(2)),
            jobs: Arc::new(crate::payload_work::Jobs::default()),
        }))
    }

    fn bind_outbound(&self, message: &mut ClientJsonRpcMessage) -> Result<(), McpError> {
        let ClientJsonRpcMessage::Request(request) = message else {
            return Ok(());
        };
        if request
            .request
            .extensions()
            .get::<Arc<RequestState>>()
            .is_none()
        {
            if !matches!(request.request.method(), "initialize" | "server/discover") {
                return Err(protocol_error());
            }
            let mut bootstrap = self.bootstrap.lock().map_err(|_| protocol_error())?;
            if bootstrap.len() >= 2 {
                return Err(protocol_error());
            }
            bootstrap.push(self.requests.prepare(&mut request.request)?);
        }
        self.requests.bind(request.id.clone(), &request.request)?;
        Ok(())
    }

    async fn decode(self: Arc<Self>, frame: RawFrame) -> Result<InboundPacket, McpError> {
        // The physical worker owns frame storage, parser scratch and request
        // state even if its async observer is cancelled.
        let jobs = Arc::clone(&self.jobs);
        jobs.run(
            rw_resources::ResourceClass::Cpu,
            rw_tools::CancellationToken::default(),
            move |_| self.decode_frame(frame),
        )
        .await
        .map_err(|_| protocol_error())?
    }

    fn decode_frame(&self, frame: RawFrame) -> Result<InboundPacket, McpError> {
        let route = decode::classify(&frame.bytes).map_err(|_| protocol_error())?;
        let mut request = if matches!(
            route.kind,
            decode::EnvelopeKind::Response | decode::EnvelopeKind::Error
        ) && route.id.is_some()
        {
            Some(self.requests.claim_response(&route)?)
        } else {
            None
        };
        // Do not overlap separately allocated routing headers across preflights.
        drop(route);
        let mut retained = None;
        let message = decode::decode(
            &frame.bytes,
            request.as_deref().map(RequestState::method),
            &mut |bytes| {
                retained = Some(Allocation::new(bytes).map_err(std::io::Error::other)?);
                Ok(())
            },
        )
        .map_err(|_| protocol_error())?;
        let decoded = Decoded {
            message,
            body: Arc::new(retained.ok_or_else(protocol_error)?),
        };
        if let ServerJsonRpcMessage::Notification(notification) = &decoded.message
            && let ServerNotification::CancelledNotification(cancelled) = &notification.notification
            && let Some(id) = &cancelled.params.request_id
        {
            request = self.requests.claim_cancellation(id)?;
        }
        if let Some(request) = &request {
            request.install_reply(Arc::clone(&decoded.body))?;
            if matches!(request.method(), "initialize" | "server/discover") {
                self.router
                    .retain_initialization(Arc::clone(&decoded.body))?;
                self.bootstrap
                    .lock()
                    .map_err(|_| protocol_error())?
                    .retain(|pending| !Arc::ptr_eq(pending, request));
            }
        }
        drop(frame);
        Ok(InboundPacket {
            message: decoded.message,
            retained: Arc::new(DeliveryRetention {
                _body: decoded.body,
                request,
                router: self.router.clone(),
            }),
        })
    }

    pub(super) fn close(&self) {
        self.requests.close();
        if let Ok(mut bootstrap) = self.bootstrap.lock() {
            bootstrap.clear();
        }
    }
}

fn protocol_error() -> McpError {
    McpError::Protocol("MCP inbound ownership or wire admission failed".into())
}

// All later correlation/publication errors destroy typed payloads before credit.
struct Decoded {
    message: ServerJsonRpcMessage,
    body: Arc<Allocation>,
}
