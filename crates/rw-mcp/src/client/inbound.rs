//! Inbound MCP authority: liveness is supported; server-directed host work is denied.
use rmcp::{
    ErrorData,
    model::{
        ClientCapabilities, ClientInfo, ClientResult, ErrorCode, Implementation,
        ServerNotification, ServerRequest,
    },
    service::{NotificationContext, RequestContext, RoleClient, Service},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// The connection's inbound capability owner. It has no filesystem, model,
/// credential, task, or interaction authority. Notification storage is one bit,
/// independent of sender volume; untrusted notification payloads are not retained.
#[derive(Clone, Default)]
pub struct McpInboundRouter {
    invalidated: Arc<AtomicBool>,
}

impl McpInboundRouter {
    #[must_use]
    pub fn catalog_valid(&self) -> bool {
        !self.invalidated.load(Ordering::Acquire)
    }

    fn request(request: &ServerRequest) -> Result<ClientResult, ErrorData> {
        match request {
            ServerRequest::PingRequest(_) => Ok(ClientResult::empty(())),
            _ => Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "MCP server-initiated host capabilities are unavailable",
                None,
            )),
        }
    }

    fn notification(&self, notification: &ServerNotification) {
        match notification {
            // Cancellation is handled by the RPC request owner. Observations
            // confer no authority and cannot allocate a backlog or expose secrets.
            ServerNotification::CancelledNotification(_)
            | ServerNotification::ProgressNotification(_)
            | ServerNotification::LoggingMessageNotification(_)
            | ServerNotification::SubscriptionsAcknowledgedNotification(_)
            | ServerNotification::TaskStatusNotification(_)
            | ServerNotification::CustomNotification(_) => {}
            // Catalog/resource changes and unrecognized state notifications
            // revoke the reviewed snapshot until explicit reconnection.
            _ => {
                self.invalidated.store(true, Ordering::Release);
            }
        }
    }
}

impl Service<RoleClient> for McpInboundRouter {
    async fn handle_request(
        &self,
        request: ServerRequest,
        _context: RequestContext<RoleClient>,
    ) -> Result<ClientResult, ErrorData> {
        Self::request(&request)
    }

    async fn handle_notification(
        &self,
        notification: ServerNotification,
        _context: NotificationContext<RoleClient>,
    ) -> Result<(), ErrorData> {
        self.notification(&notification);
        Ok(())
    }

    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("rottweiler", env!("CARGO_PKG_VERSION")),
        )
    }
}

#[cfg(test)]
mod tests;
