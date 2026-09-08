use super::{
    envelope::{Envelope, EnvelopeKind, EnvelopeRoute},
    invalid, json_error,
};
use rmcp::model::{
    self as m, ServerJsonRpcMessage, ServerNotification, ServerRequest, ServerResult,
};
use serde::{
    Deserialize, Deserializer as _,
    de::{self, DeserializeOwned, Visitor},
};
use serde_json::value::RawValue;
use std::io;

#[derive(Deserialize)]
struct ResultTag<'a> {
    #[serde(default, borrow, rename = "resultType")]
    kind: Option<&'a RawValue>,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum ResultKind {
    InputRequired,
    Task,
    Other,
}
fn result_kind(body: &RawValue, method: Option<&str>) -> io::Result<Option<ResultKind>> {
    struct Kind;
    impl Visitor<'_> for Kind {
        type Value = ResultKind;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a result type string")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<ResultKind, E> {
            Ok(match value {
                "input_required" => ResultKind::InputRequired,
                "task" => ResultKind::Task,
                _ => ResultKind::Other,
            })
        }
    }
    if !matches!(
        method,
        Some("tools/call" | "resources/read" | "prompts/get")
    ) {
        return Ok(None);
    }
    if !body.get().trim_start().starts_with('{') {
        return Ok(None);
    }
    let tag: ResultTag<'_> = serde_json::from_str(body.get()).map_err(json_error)?;
    tag.kind
        .map(|kind| {
            serde_json::Deserializer::from_str(kind.get())
                .deserialize_str(Kind)
                .map_err(json_error)
        })
        .transpose()
}
pub(super) fn request_graph(
    envelope: &Envelope<'_>,
    route: &EnvelopeRoute<'_>,
    expected: Option<&str>,
) -> io::Result<bool> {
    Ok(matches!(route.kind, EnvelopeKind::Request)
        || envelope
            .result
            .map(|body| result_kind(body, expected))
            .transpose()?
            .flatten()
            == Some(ResultKind::InputRequired))
}
fn parse<T: DeserializeOwned>(input: &[u8]) -> io::Result<T> {
    serde_json::from_slice(input).map_err(json_error)
}
fn body<T: DeserializeOwned>(input: &RawValue) -> io::Result<T> {
    serde_json::from_str(input.get()).map_err(json_error)
}

pub(super) fn decode(
    envelope: &Envelope<'_>,
    route: &EnvelopeRoute<'_>,
    expected: Option<&str>,
) -> io::Result<ServerJsonRpcMessage> {
    match route.kind {
        EnvelopeKind::Response => {
            let input = envelope
                .result
                .ok_or_else(|| invalid("MCP result missing"))?;
            let result = result(
                input,
                expected.ok_or_else(|| invalid("MCP result method missing"))?,
            )?;
            Ok(ServerJsonRpcMessage::response(
                result,
                route
                    .id
                    .ok_or_else(|| invalid("MCP result ID missing"))?
                    .owned()?,
            ))
        }
        EnvelopeKind::Error => Ok(ServerJsonRpcMessage::error(
            body(envelope.error.ok_or_else(|| invalid("MCP error missing"))?)?,
            route
                .id
                .map(super::envelope::BorrowedId::owned)
                .transpose()?,
        )),
        EnvelopeKind::Request => Ok(ServerJsonRpcMessage::request(
            request(
                envelope.input,
                route
                    .method
                    .as_deref()
                    .ok_or_else(|| invalid("MCP request method missing"))?,
            )?,
            route
                .id
                .ok_or_else(|| invalid("MCP request ID missing"))?
                .owned()?,
        )),
        EnvelopeKind::Notification => Ok(ServerJsonRpcMessage::notification(notification(
            envelope.input,
            route
                .method
                .as_deref()
                .ok_or_else(|| invalid("MCP notification method missing"))?,
        )?)),
    }
}

fn result(input: &RawValue, method: &str) -> io::Result<ServerResult> {
    let kind = result_kind(input, Some(method))?;
    match (method, kind) {
        ("tools/call" | "resources/read" | "prompts/get", Some(ResultKind::InputRequired)) => {
            return body(input).map(ServerResult::InputRequiredResult);
        }
        ("tools/call", Some(ResultKind::Task)) => {
            return body(input).map(ServerResult::CreateTaskResult);
        }
        _ => {}
    }
    match method {
        "initialize" => body(input).map(ServerResult::InitializeResult),
        "server/discover" => body(input).map(ServerResult::DiscoverResult),
        "tools/list" => body(input).map(ServerResult::ListToolsResult),
        "resources/list" => body(input).map(ServerResult::ListResourcesResult),
        "resources/templates/list" => body(input).map(ServerResult::ListResourceTemplatesResult),
        "prompts/list" => body(input).map(ServerResult::ListPromptsResult),
        "tools/call" => body(input).map(ServerResult::CallToolResult),
        "resources/read" => body(input).map(ServerResult::ReadResourceResult),
        "prompts/get" => body(input).map(ServerResult::GetPromptResult),
        "completion/complete" => body(input).map(ServerResult::CompleteResult),
        "subscriptions/listen" => body(input).map(ServerResult::SubscriptionsListenResult),
        "tasks/get" => body(input).map(ServerResult::GetTaskResult),
        "tasks/update" | "tasks/cancel" => body(input).map(ServerResult::TaskAckResult),
        "ping" | "logging/setLevel" | "resources/subscribe" | "resources/unsubscribe" => {
            body(input).map(ServerResult::EmptyResult)
        }
        _ => body(input).map(ServerResult::CustomResult),
    }
}

#[allow(deprecated)] // Preserve external sampling requests for explicit capability rejection.
fn request(input: &[u8], method: &str) -> io::Result<ServerRequest> {
    match method {
        "ping" => parse::<m::PingRequest>(input).map(ServerRequest::PingRequest),
        "sampling/createMessage" => {
            parse::<m::CreateMessageRequest>(input).map(ServerRequest::CreateMessageRequest)
        }
        "roots/list" => parse::<m::ListRootsRequest>(input).map(ServerRequest::ListRootsRequest),
        "elicitation/create" => parse::<m::ElicitRequest>(input).map(ServerRequest::ElicitRequest),
        _ => parse::<m::CustomRequest>(input).map(ServerRequest::CustomRequest),
    }
}
#[allow(deprecated)] // External logging notifications remain accepted observations.
fn notification(input: &[u8], method: &str) -> io::Result<ServerNotification> {
    match method {
        "notifications/cancelled" => {
            parse::<m::CancelledNotification>(input).map(ServerNotification::CancelledNotification)
        }
        "notifications/progress" => {
            parse::<m::ProgressNotification>(input).map(ServerNotification::ProgressNotification)
        }
        "notifications/message" => parse::<m::LoggingMessageNotification>(input)
            .map(ServerNotification::LoggingMessageNotification),
        "notifications/resources/updated" => parse::<m::ResourceUpdatedNotification>(input)
            .map(ServerNotification::ResourceUpdatedNotification),
        "notifications/resources/list_changed" => {
            parse::<m::ResourceListChangedNotification>(input)
                .map(ServerNotification::ResourceListChangedNotification)
        }
        "notifications/tools/list_changed" => parse::<m::ToolListChangedNotification>(input)
            .map(ServerNotification::ToolListChangedNotification),
        "notifications/prompts/list_changed" => parse::<m::PromptListChangedNotification>(input)
            .map(ServerNotification::PromptListChangedNotification),
        "notifications/subscriptions/acknowledged" => {
            parse::<m::SubscriptionsAcknowledgedNotification>(input)
                .map(ServerNotification::SubscriptionsAcknowledgedNotification)
        }
        "notifications/tasks" => parse::<m::TaskStatusNotification>(input)
            .map(ServerNotification::TaskStatusNotification),
        _ => parse::<m::CustomNotification>(input).map(ServerNotification::CustomNotification),
    }
}
