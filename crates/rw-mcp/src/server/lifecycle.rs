//! MCP version/lifecycle decisions for the tools-only Rottweiler endpoint.
use super::RottweilerMcpServer;
use crate::McpResponse;
use rmcp::{
    ErrorData,
    model::{self as m, ClientRequest, GetMeta as _, ProtocolVersion, ServerResult},
};

#[derive(Default)]
pub(super) struct Lifecycle {
    version: Option<ProtocolVersion>,
    inline: bool,
}
pub(super) struct Negotiated {
    pub legacy: bool,
    pub initialize: Option<m::InitializeResult>,
}
impl Lifecycle {
    pub fn admit(&mut self, request: &ClientRequest) -> Result<Negotiated, ErrorData> {
        if let ClientRequest::InitializeRequest(request) = request {
            let mut info = RottweilerMcpServer::get_info();
            if ProtocolVersion::KNOWN_VERSIONS.contains(&request.params.protocol_version) {
                info.protocol_version = request.params.protocol_version.clone();
            }
            self.version = Some(info.protocol_version.clone());
            self.inline = false;
            return Ok(Negotiated {
                legacy: info.protocol_version < ProtocolVersion::V_2026_07_28,
                initialize: Some(info),
            });
        }
        let meta = request.get_meta();
        let version = meta.protocol_version();
        let discovery = matches!(request, ClientRequest::DiscoverRequest(_));
        let first = self.version.is_none() && !self.inline;
        let pre_init_ping = first && matches!(request, ClientRequest::PingRequest(_));
        if pre_init_ping {
            return Ok(Negotiated {
                legacy: true,
                initialize: None,
            });
        }
        let requires_meta = !pre_init_ping
            && (self.inline
                || first
                || discovery
                || version
                    .as_ref()
                    .is_some_and(|v| v >= &ProtocolVersion::V_2026_07_28));
        if let Some(version) = version.as_ref()
            && !ProtocolVersion::KNOWN_VERSIONS.contains(version)
        {
            return Err(ErrorData::unsupported_protocol_version(
                version.clone(),
                ProtocolVersion::KNOWN_VERSIONS,
            ));
        }
        if requires_meta {
            let missing = meta.missing_required_keys(&ProtocolVersion::V_2026_07_28);
            if !missing.is_empty() {
                return Err(ErrorData::invalid_params(
                    format!(
                        "request _meta is missing or has malformed required fields: {}",
                        missing.join(", ")
                    ),
                    None,
                ));
            }
        }
        if first && !pre_init_ping {
            self.inline = true;
        }
        Ok(Negotiated {
            legacy: !requires_meta
                && version
                    .as_ref()
                    .or(self.version.as_ref())
                    .is_none_or(|v| v < &ProtocolVersion::V_2026_07_28),
            initialize: None,
        })
    }
}

pub(super) async fn execute(
    server: &RottweilerMcpServer,
    request: ClientRequest,
    negotiated: Negotiated,
    decoded: std::sync::Arc<crate::payload_work::Allocation>,
) -> Result<McpResponse<ServerResult>, ErrorData> {
    let mut result = match request {
        ClientRequest::InitializeRequest(_) => ServerResult::InitializeResult(
            negotiated
                .initialize
                .ok_or_else(|| ErrorData::internal_error("MCP negotiation missing", None))?,
        ),
        ClientRequest::DiscoverRequest(_) => {
            ServerResult::DiscoverResult(m::DiscoverResult::from_server_info(
                ProtocolVersion::KNOWN_VERSIONS.to_vec(),
                RottweilerMcpServer::get_info(),
            ))
        }
        ClientRequest::PingRequest(_) if negotiated.legacy => ServerResult::empty(()),
        ClientRequest::CompleteRequest(_) => {
            ServerResult::CompleteResult(m::CompleteResult::default())
        }
        ClientRequest::ListToolsRequest(_) => ServerResult::ListToolsResult(
            m::ListToolsResult::with_all_items(RottweilerMcpServer::builtin_tools()),
        ),
        ClientRequest::ListPromptsRequest(_) => {
            ServerResult::ListPromptsResult(m::ListPromptsResult::default())
        }
        ClientRequest::ListResourcesRequest(_) => {
            ServerResult::ListResourcesResult(m::ListResourcesResult::default())
        }
        ClientRequest::ListResourceTemplatesRequest(_) => {
            ServerResult::ListResourceTemplatesResult(m::ListResourceTemplatesResult::default())
        }
        ClientRequest::CallToolRequest(request) => {
            let response = server.execute_tool(request.params, decoded).await?;
            let mut value = ServerResult::from(response.value);
            if negotiated.legacy {
                value.strip_result_type_for_legacy_peer();
            }
            return Ok(McpResponse::wire(value, response.retained));
        }
        other => {
            return Err(ErrorData::new(
                m::ErrorCode::METHOD_NOT_FOUND,
                method(&other),
                None,
            ));
        }
    };
    if negotiated.legacy {
        result.strip_result_type_for_legacy_peer();
    }
    Ok(McpResponse::wire(result, Vec::new()))
}
fn method(request: &ClientRequest) -> String {
    // External unknown methods are already bounded by the raw envelope decoder.
    match request {
        ClientRequest::CustomRequest(request) => request.method.clone(),
        _ => "Method not found".to_owned(),
    }
}
