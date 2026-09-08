//! Inbound host work is consumed under its physical decoded-body owner.
use super::{decode::DecodedMessage, requests::RequestState};
use crate::{McpInboundRouter, payload_work::Allocation};
use rmcp::{
    ErrorData,
    model::{ClientJsonRpcMessage, ErrorCode, ServerJsonRpcMessage, ServerNotification},
};
use std::sync::Arc;

pub(super) struct DeliveryRetention {
    pub(super) _body: Arc<Allocation>,
    pub(super) request: Option<Arc<RequestState>>,
    pub(super) router: McpInboundRouter,
}

pub(super) struct InboundPacket {
    pub(super) message: DecodedMessage,
    pub(super) retained: Arc<DeliveryRetention>,
}

pub(super) enum Delivery {
    Response(ServerJsonRpcMessage, Arc<DeliveryRetention>),
    Reply(ClientJsonRpcMessage, Arc<DeliveryRetention>),
    Consumed,
}

impl InboundPacket {
    pub(super) fn dispatch(self) -> Delivery {
        match self.message {
            DecodedMessage::DeniedRequest(id) => Delivery::Reply(
                ClientJsonRpcMessage::error(McpInboundRouter::unsupported_request(), Some(id)),
                self.retained,
            ),
            DecodedMessage::Protocol(ServerJsonRpcMessage::Request(request)) => {
                let reply = match McpInboundRouter::request(&request.request) {
                    Ok(result) => ClientJsonRpcMessage::response(result, request.id),
                    Err(error) => ClientJsonRpcMessage::error(error, Some(request.id)),
                };
                drop(request.request);
                Delivery::Reply(reply, self.retained)
            }
            DecodedMessage::Protocol(ServerJsonRpcMessage::Notification(notification)) => {
                let mut reply = None;
                if let ServerNotification::CancelledNotification(cancelled) =
                    &notification.notification
                    && self.retained.request.is_some()
                {
                    // Complete the exact pending rmcp responder without moving
                    // this body into an opaque spawned notification handler.
                    reply = Some(ServerJsonRpcMessage::error(
                        ErrorData::new(
                            ErrorCode::INTERNAL_ERROR,
                            "MCP server cancelled this request",
                            None,
                        ),
                        cancelled.params.request_id.clone(),
                    ));
                }
                self.retained
                    .router
                    .notification(&notification.notification);
                drop(notification);
                reply.map_or(Delivery::Consumed, |reply| {
                    Delivery::Response(reply, self.retained)
                })
            }
            DecodedMessage::Protocol(
                message @ (ServerJsonRpcMessage::Response(_) | ServerJsonRpcMessage::Error(_)),
            ) => {
                // The last-packet guard fences route-or-drop. The owned request
                // task independently retains the same request state through its
                // oneshot result and conversion to the public response carrier.
                Delivery::Response(message, self.retained)
            }
        }
    }
}

/// Keep the actual pending send before its decoded reply owner even when the
/// spawned control task is destroyed before its first poll.
pub(super) fn control(
    send: impl Future<Output = std::io::Result<()>> + Send + 'static,
    retained: Arc<DeliveryRetention>,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    let work = Control {
        send: Box::pin(send),
        _retained: retained,
    };
    tokio::spawn(work.run())
}
struct Control {
    send: std::pin::Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>,
    _retained: Arc<DeliveryRetention>,
}
impl Control {
    async fn run(mut self) -> std::io::Result<()> {
        self.send.as_mut().await
    }
}
