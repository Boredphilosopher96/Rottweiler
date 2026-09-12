//! Exact request states outlive rmcp queueing, response routing and conversion.
use super::ingress::requests::RequestState;
use super::{RmcpClient, protocol};
use crate::{McpError, McpResponse, McpResponseSlot};
use rmcp::{
    model::{ClientRequest, ServerResult},
    service::{PeerRequestOptions, RoleClient},
};
use std::sync::Arc;

struct RequestWork {
    request: Option<ClientRequest>,
    peer: rmcp::Peer<RoleClient>,
    state: Arc<RequestState>,
    _slot: McpResponseSlot,
}
impl RequestWork {
    async fn run(mut self) -> Result<McpResponse<ServerResult>, McpError> {
        let request = self.request.take().ok_or_else(super::protocol_failure)?;
        let handle = self
            .peer
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
            .map_err(protocol)?;
        let result = handle.await_response().await.map_err(protocol)?;
        let retained = self.state.reply_retention()?;
        let reply = McpResponse::wire(result, vec![retained]);
        // All rmcp Peer holders are destroyed before their initialization guard
        // in state. The response has its own independent decoded-body lease.
        drop(self);
        Ok(reply)
    }
}

impl RmcpClient {
    pub(super) async fn request(
        &self,
        mut request: ClientRequest,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<ServerResult>, McpError> {
        let work = RequestWork {
            peer: self.peer()?,
            state: self.ingress.requests.prepare(&mut request)?,
            request: Some(request),
            _slot: slot,
        };
        // RequestHandle has no drop cancellation contract. This physical task
        // remains its sole observer even after the manager's waiting caller goes
        // away; connection retirement settles pending responders on timeout.
        tokio::spawn(work.run())
            .await
            .map_err(|_| super::protocol_failure())?
    }
}

pub(super) enum ResultKind {
    Tool,
    Resource,
    Prompt,
}
struct ValueWork {
    response: McpResponse<ServerResult>,
    kind: ResultKind,
}
impl ValueWork {
    fn run(self) -> Result<McpResponse<serde_json::Value>, McpError> {
        let value = match (self.kind, self.response.value) {
            (ResultKind::Tool, ServerResult::CallToolResult(value)) => serde_json::to_value(value),
            (ResultKind::Resource, ServerResult::ReadResourceResult(value)) => {
                serde_json::to_value(value)
            }
            (ResultKind::Prompt, ServerResult::GetPromptResult(value)) => {
                serde_json::to_value(value)
            }
            _ => return Err(super::protocol_failure()),
        }
        .map_err(protocol)?;
        Ok(McpResponse::wire(value, self.response.retained))
    }
}
impl RmcpClient {
    pub(super) async fn value_request(
        &self,
        request: ClientRequest,
        kind: ResultKind,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<serde_json::Value>, McpError> {
        let work = ValueWork {
            response: self.request(request, slot).await?,
            kind,
        };
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
            .await
            .map_err(protocol)?
    }
}
