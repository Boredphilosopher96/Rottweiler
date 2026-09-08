//! SEP-2243 projection from borrowed typed requests and retained annotations.
use crate::McpError;
use rmcp::model::{
    ClientJsonRpcMessage, ClientNotification, ClientRequest, ConstString, GetMeta, ProtocolVersion,
};
use serde_json::Value;
use std::borrow::Cow;

mod annotations;
mod encoding;
#[cfg(test)]
mod tests;
pub(super) use annotations::ToolHeaderAnnotations;

use crate::http_io::{
    MCP_HTTP_MAX_HEADER_BYTES as MAX_BYTES, MCP_HTTP_MAX_HEADER_VALUE_BYTES as MAX_VALUE_BYTES,
    MCP_HTTP_MAX_HEADERS as MAX_HEADERS,
};
const PROTOCOL_META: &str = "io.modelcontextprotocol/protocolVersion";

/// The caller holds 64KiB construction credit through this vector's retirement
/// or transfer to the physical HTTP request. Base/custom headers are checked by
/// that owner together with this projection before any network effect.
pub(super) fn project(
    message: &ClientJsonRpcMessage,
    annotations: Option<&ToolHeaderAnnotations>,
    fallback: &ProtocolVersion,
) -> Result<Vec<(String, String)>, McpError> {
    // Read only the scalar metadata field; SDK protocol_version() first clones
    // its generic Value, which is unnecessary for a borrowed routing decision.
    let version = match message {
        ClientJsonRpcMessage::Request(request) => request
            .request
            .get_meta()
            .get(PROTOCOL_META)
            .and_then(Value::as_str)
            .unwrap_or(fallback.as_str()),
        _ => fallback.as_str(),
    };
    let mut output = Headers {
        pairs: Vec::with_capacity(MAX_HEADERS),
        bytes: 0,
    };
    output.push("mcp-protocol-version", version, false)?;
    if version < ProtocolVersion::STANDARD_HEADERS.as_str() {
        return Ok(output.pairs);
    }
    let fields = fields(message);
    if let Some(method) = fields.method {
        output.push("mcp-method", method, false)?;
    }
    if let Some(name) = fields.name {
        output.push("mcp-name", name, true)?;
    }
    if fields.method == Some("tools/call")
        && let (Some(annotations), Some(arguments)) = (annotations, fields.arguments)
    {
        for (property, header) in &annotations.pairs {
            let Some(value) = arguments.get(property) else {
                continue;
            };
            let value = match value {
                Value::String(value) => Cow::Borrowed(value.as_str()),
                Value::Bool(true) => Cow::Borrowed("true"),
                Value::Bool(false) => Cow::Borrowed("false"),
                Value::Number(value) => Cow::Owned(value.to_string()),
                _ => continue,
            };
            output.push(header, &value, true)?;
        }
    }
    Ok(output.pairs)
}

struct Headers {
    pairs: Vec<(String, String)>,
    bytes: usize,
}
impl Headers {
    fn push(&mut self, name: &str, value: &str, encoded: bool) -> Result<(), McpError> {
        let value_bytes = if encoded {
            encoding::encoded_len(value)?
        } else {
            value.len()
        };
        let bytes = self
            .bytes
            .checked_add(name.len())
            .and_then(|n| n.checked_add(value_bytes))
            .ok_or_else(invalid)?;
        if self.pairs.len() == MAX_HEADERS || value_bytes > MAX_VALUE_BYTES || bytes > MAX_BYTES {
            return Err(invalid());
        }
        // Pinned rmcp skips invalid ordinary method/version header values. The
        // standardized name/parameter encoding makes those valid HTTP values.
        if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
            return Ok(());
        }
        let value = if encoded {
            encoding::encode(value)?
        } else {
            value.to_owned()
        };
        if http::HeaderValue::from_str(&value).is_err() {
            return Ok(());
        }
        self.bytes = bytes;
        self.pairs.push((name.to_ascii_lowercase(), value));
        Ok(())
    }
}

#[derive(Default)]
struct Fields<'a> {
    method: Option<&'a str>,
    name: Option<&'a str>,
    arguments: Option<&'a rmcp::model::JsonObject>,
}
#[allow(deprecated)] // External resource subscription requests retain their wire headers.
fn fields(message: &ClientJsonRpcMessage) -> Fields<'_> {
    match message {
        ClientJsonRpcMessage::Request(request) => {
            let request = &request.request;
            let mut fields = Fields {
                method: Some(request.method()),
                ..Fields::default()
            };
            fields.name = match request {
                ClientRequest::CallToolRequest(request) => {
                    fields.arguments = request.params.arguments.as_ref();
                    Some(request.params.name.as_ref())
                }
                ClientRequest::GetPromptRequest(request) => Some(&request.params.name),
                ClientRequest::ReadResourceRequest(request) => Some(&request.params.uri),
                ClientRequest::SubscribeRequest(request) => Some(&request.params.uri),
                ClientRequest::UnsubscribeRequest(request) => Some(&request.params.uri),
                ClientRequest::GetTaskRequest(request) => Some(&request.params.task_id),
                ClientRequest::UpdateTaskRequest(request) => Some(&request.params.task_id),
                ClientRequest::CancelTaskRequest(request) => Some(&request.params.task_id),
                ClientRequest::CustomRequest(request) => {
                    return custom(&request.method, request.params.as_ref());
                }
                _ => None,
            };
            fields
        }
        ClientJsonRpcMessage::Notification(notification) => {
            let method = match &notification.notification {
                ClientNotification::CancelledNotification(value) => value.method.as_str(),
                ClientNotification::ProgressNotification(value) => value.method.as_str(),
                ClientNotification::InitializedNotification(value) => value.method.as_str(),
                ClientNotification::RootsListChangedNotification(value) => value.method.as_str(),
                ClientNotification::CustomNotification(value) => {
                    return custom(&value.method, value.params.as_ref());
                }
                _ => return Fields::default(),
            };
            Fields {
                method: Some(method),
                ..Fields::default()
            }
        }
        _ => Fields::default(),
    }
}
fn custom<'a>(method: &'a str, params: Option<&'a Value>) -> Fields<'a> {
    let key = match method {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" | "resources/subscribe" | "resources/unsubscribe" => Some("uri"),
        "tasks/get" | "tasks/update" | "tasks/cancel" => Some("taskId"),
        _ => None,
    };
    Fields {
        method: Some(method),
        name: key
            .and_then(|key| params.and_then(|params| params.get(key)))
            .and_then(Value::as_str),
        arguments: params
            .and_then(|params| params.get("arguments"))
            .and_then(Value::as_object),
    }
}
fn invalid() -> McpError {
    McpError::Protocol("MCP HTTP header projection admission failed".into())
}
