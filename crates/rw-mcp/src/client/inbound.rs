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
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// The connection's inbound capability owner. It has no filesystem, model,
/// credential, task, or interaction authority. Notification storage is one bit,
/// independent of sender volume; untrusted notification payloads are not retained.
#[derive(Clone, Default)]
pub struct McpInboundRouter {
    invalidated: Arc<AtomicBool>,
    initialization: Arc<Mutex<Vec<Arc<crate::payload_work::Allocation>>>>,
}

impl McpInboundRouter {
    pub(super) fn retain_initialization(
        &self,
        retained: Arc<crate::payload_work::Allocation>,
    ) -> Result<(), crate::McpError> {
        let mut initialization = self
            .initialization
            .lock()
            .map_err(|_| crate::McpError::Protocol("MCP initialization owner poisoned".into()))?;
        if initialization.len() >= 2 {
            return Err(crate::McpError::Protocol(
                "MCP initialization response admission exceeded".into(),
            ));
        }
        initialization.push(retained);
        Ok(())
    }

    #[must_use]
    pub fn catalog_valid(&self) -> bool {
        !self.invalidated.load(Ordering::Acquire)
    }

    pub(super) fn request(request: &ServerRequest) -> Result<ClientResult, ErrorData> {
        match request {
            ServerRequest::PingRequest(_) => Ok(ClientResult::empty(())),
            _ => Err(Self::unsupported_request()),
        }
    }

    pub(super) fn unsupported_request() -> ErrorData {
        ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            "MCP server-initiated host capabilities are unavailable",
            None,
        )
    }

    pub(super) fn notification(&self, notification: &ServerNotification) {
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
        context: RequestContext<RoleClient>,
    ) -> Result<ClientResult, ErrorData> {
        let response = Self::request(&request);
        drop(request);
        drop(context);
        response
    }

    async fn handle_notification(
        &self,
        notification: ServerNotification,
        context: NotificationContext<RoleClient>,
    ) -> Result<(), ErrorData> {
        self.notification(&notification);
        drop(notification);
        drop(context);
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
