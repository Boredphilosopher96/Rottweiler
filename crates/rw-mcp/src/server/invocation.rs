//! Wrapper arguments are validated in admitted CPU work before bridge invocation.
use super::{
    CallToolRequestParams, CreateSession, MAX_SERVER_ARGUMENTS, McpProtocolError, SendMessage,
    ToolCall, parse,
};
use crate::{McpResponse, payload_work::Allocation};
use std::sync::Arc;

pub(super) enum Invocation {
    Tool(ToolCall),
    Create(CreateSession),
    List,
    Send(SendMessage),
    Oversized,
    Unknown,
}
struct Input {
    request: CallToolRequestParams,
    decoded: Arc<Allocation>,
}
impl Input {
    fn run(mut self) -> Result<McpResponse<Invocation>, McpProtocolError> {
        let mut count = rw_types::json_encoding::JsonWriter::count(MAX_SERVER_ARGUMENTS);
        let value = if count.serialize(&self.request.arguments).is_err() {
            Invocation::Oversized
        } else {
            match self.request.name.as_ref() {
                "rottweiler_tools_call" => Invocation::Tool(parse(&mut self.request)?),
                "rottweiler_sessions_create" => Invocation::Create(parse(&mut self.request)?),
                "rottweiler_sessions_list" => Invocation::List,
                "rottweiler_sessions_send" => Invocation::Send(parse(&mut self.request)?),
                _ => Invocation::Unknown,
            }
        };
        drop(self.request);
        Ok(McpResponse::wire(value, vec![self.decoded]))
    }
}
pub(super) async fn prepare(
    request: CallToolRequestParams,
    decoded: Arc<Allocation>,
) -> Result<McpResponse<Invocation>, McpProtocolError> {
    let work = Input { request, decoded };
    rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
        .await
        .map_err(|_| {
            McpProtocolError::internal_error("MCP argument preparation worker failed", None)
        })?
}
