//! Outbound encoding and session metadata use the same admitted physical input.
use super::{
    state::{Schema, Shared, invalid, valid_id},
    worker::Response,
};
use crate::client::ingress::{
    decode::DecodedMessage,
    http_headers::{self, ToolHeaderAnnotations},
    message::InboundPacket,
};
use crate::{McpError, McpHttpBody, McpHttpMethod, payload_work::Allocation};
use rmcp::model::{
    ClientJsonRpcMessage, ClientNotification, ClientRequest, ServerJsonRpcMessage, ServerResult,
};
use std::sync::Arc;

struct Encoded {
    headers: Vec<(String, String)>,
    body: McpHttpBody,
    request: bool,
    initialized: bool,
}
impl Shared {
    pub(super) async fn post(
        self: &Arc<Self>,
        message: ClientJsonRpcMessage,
    ) -> Result<(), McpError> {
        let shared = Arc::clone(self);
        let encoded = self
            .ingress
            .jobs
            .run(
                rw_resources::ResourceClass::Cpu,
                self.stopped.clone(),
                move |_| shared.encode(&message),
            )
            .await??;
        let response = self
            .request(McpHttpMethod::Post, encoded.headers, encoded.body)
            .await?;
        self.capture_session(&response)?;
        if response.status == 202 || response.status == 204 {
            response.discard();
        } else {
            // A JSON-RPC error remains a protocol response even on HTTP 4xx/5xx.
            // consume accepts only the error envelope on that status path.
            self.consume(response, encoded.request).await?;
        }
        if encoded.initialized {
            self.session.lock().map_err(|_| invalid())?.initialized = true;
            self.changed.notify_one();
        }
        Ok(())
    }
    fn encode(&self, message: &ClientJsonRpcMessage) -> Result<Encoded, McpError> {
        let limit = super::super::frame::STDIO_FRAME_BYTES;
        let mut count = rw_types::json_encoding::JsonWriter::count(limit);
        count.serialize(&message).map_err(|_| invalid())?;
        let capacity = count.written();
        let retained = Arc::new(Allocation::new(
            capacity.checked_add(64 * 1024).ok_or_else(invalid)?,
        )?);
        let headers = {
            let session = self.session.lock().map_err(|_| invalid())?;
            let annotations = match &message {
                ClientJsonRpcMessage::Request(request) => match &request.request {
                    ClientRequest::CallToolRequest(call) => session
                        .schemas
                        .get(call.params.name.as_ref())
                        .map(|schema| &schema.annotations),
                    _ => None,
                },
                _ => None,
            };
            http_headers::project(message, annotations, &session.version)?
        };
        let headers = self.headers(headers, None, true)?;
        let mut bytes = Vec::with_capacity(capacity);
        rw_types::json_encoding::JsonWriter::buffer(&mut bytes, capacity, 0)
            .map_err(|_| invalid())?
            .serialize(&message)
            .map_err(|_| invalid())?;
        let request = matches!(&message, ClientJsonRpcMessage::Request(_));
        let initialized = matches!(&message, ClientJsonRpcMessage::Notification(notification) if matches!(&notification.notification, ClientNotification::InitializedNotification(_)));
        Ok(Encoded {
            headers,
            body: McpHttpBody::new(bytes, retained),
            request,
            initialized,
        })
    }
    fn capture_session(&self, response: &Response) -> Result<(), McpError> {
        let mut values = response
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"));
        let value = values.next().map(|(_, value)| value);
        if values.next().is_some() {
            return Err(invalid());
        }
        if let Some(value) = value {
            if !valid_id(value, 256) {
                return Err(invalid());
            }
            let mut session = self.session.lock().map_err(|_| invalid())?;
            if session
                .id
                .as_ref()
                .is_some_and(|previous| previous != value)
            {
                return Err(invalid());
            }
            if session.id.is_none() {
                session.id = Some(value.clone());
            }
        }
        Ok(())
    }
    pub(super) fn observe(&self, packet: &mut InboundPacket) -> Result<(), McpError> {
        let DecodedMessage::Protocol(ServerJsonRpcMessage::Response(response)) =
            &mut packet.message
        else {
            return Ok(());
        };
        match &mut response.result {
            ServerResult::InitializeResult(result) => {
                self.session.lock().map_err(|_| invalid())?.version =
                    result.protocol_version.clone();
            }
            ServerResult::ListToolsResult(result) => {
                let mut session = self.session.lock().map_err(|_| invalid())?;
                if packet
                    .retained
                    .request
                    .as_ref()
                    .is_some_and(|request| request.catalog_start())
                {
                    session.schemas.clear();
                }
                let mut index = 0;
                while index < result.tools.len() {
                    let tool = &result.tools[index];
                    let Some(annotations) = ToolHeaderAnnotations::extract(&tool.input_schema)?
                    else {
                        result.tools.remove(index);
                        continue;
                    };
                    if !session.schemas.contains_key(tool.name.as_ref())
                        && session.schemas.len() == 256
                    {
                        return Err(invalid());
                    }
                    let name = Allocation::new(
                        tool.name
                            .len()
                            .checked_mul(2)
                            .and_then(|bytes| bytes.checked_add(256))
                            .ok_or_else(invalid)?,
                    )?;
                    session.schemas.insert(
                        tool.name.to_string(),
                        Schema {
                            annotations,
                            _name: name,
                        },
                    );
                    index += 1;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
