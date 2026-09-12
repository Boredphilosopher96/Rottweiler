//! A bounded external SDK server exercises the production guarded HTTP client.
use rmcp::{
    ErrorData, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::json;

pub(super) struct ExternalHttpFixture;
fn fixture_tool() -> Tool {
    Tool::new(
        "rottweiler_tools_call",
        "Echo one bounded interoperability fixture message",
        json!({"type":"object"})
            .as_object()
            .expect("schema object")
            .clone(),
    )
}
impl ServerHandler for ExternalHttpFixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        (name == "rottweiler_tools_call").then(fixture_tool)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![fixture_tool()]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let invalid = || ErrorData::invalid_params("invalid bounded fixture call", None);
        if request.name != "rottweiler_tools_call" {
            return Err(invalid());
        }
        let arguments = request.arguments.as_ref().ok_or_else(invalid)?;
        if arguments.get("name").and_then(serde_json::Value::as_str) != Some("echo") {
            return Err(invalid());
        }
        let message = arguments
            .get("arguments")
            .and_then(|value| value.get("message"))
            .and_then(serde_json::Value::as_str)
            .filter(|message| message.len() <= 4096)
            .ok_or_else(invalid)?;
        Ok(CallToolResult::structured(json!({"message":message})).into())
    }
}
