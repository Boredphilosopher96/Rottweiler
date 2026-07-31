use std::sync::Arc;

use rmcp::{
    ErrorData, ServerHandler, ServiceExt as _,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        GetPromptRequestParams, GetPromptResponse, GetPromptResult, Implementation,
        ListPromptsResult, ListResourcesResult, ListToolsResult, Prompt, PromptMessage,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
        ResourceContents, Role, ServerCapabilities, ServerInfo, Tool,
    },
};
use serde_json::json;

#[derive(Clone)]
struct Fixture {
    profile: String,
}

impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("rw-mcp-fixture", "1"))
    }

    async fn list_tools(
        &self,
        _: Option<rmcp::model::PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let suffix = self.profile.replace('-', "_");
        Ok(ListToolsResult::with_all_items(vec![
                Tool::new(format!("echo_{suffix}"), format!("Echo one value for {}", self.profile), Arc::new(json!({"type":"object","required":["value"],"properties":{"value":{"type":"string","maxLength":4096}}}).as_object().cloned().unwrap_or_default())),
                Tool::new(format!("search_{suffix}"), format!("Search bounded {} records", self.profile), Arc::new(json!({"type":"object","required":["query"],"properties":{"query":{"type":"string"},"filters":{"type":"object","additionalProperties":{"type":"string"}},"limit":{"type":"integer","minimum":1,"maximum":100}}}).as_object().cloned().unwrap_or_default())),
                Tool::new(format!("create_{suffix}"), format!("Create one {} record", self.profile), Arc::new(json!({"type":"object","required":["title","body"],"properties":{"title":{"type":"string"},"body":{"type":"string"},"labels":{"type":"array","items":{"type":"string"}},"metadata":{"type":"object"}}}).as_object().cloned().unwrap_or_default())),
            ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&request.arguments).unwrap_or_default(),
        )])
        .into())
    }

    async fn list_resources(
        &self,
        _: Option<rmcp::model::PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(
                format!("memory://{}/guide", self.profile),
                format!("{} guide", self.profile),
            )
            .with_description(format!("Fixture guide for {}", self.profile)),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        Ok(ReadResourceResult::new(vec![ResourceContents::text("fixture", request.uri)]).into())
    }

    async fn list_prompts(
        &self,
        _: Option<rmcp::model::PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            format!("review_{}", self.profile.replace('-', "_")),
            Some(format!("Review {}", self.profile)),
            None,
        )]))
    }

    async fn get_prompt(
        &self,
        _: GetPromptRequestParams,
        _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, "review")]).into())
    }
}

#[tokio::main]
async fn main() {
    let profile = std::env::var("RW_MCP_PROFILE").unwrap_or_else(|_| "generic".to_owned());
    if let Ok(path) = std::env::var("RW_MCP_PID_FILE") {
        let _ = std::fs::write(path, std::process::id().to_string());
    }
    run_policy_probes();
    let Ok(service) = Fixture { profile }.serve(rmcp::transport::stdio()).await else {
        return;
    };
    let _ = service.waiting().await;
}

fn run_policy_probes() {
    if let Ok(path) = std::env::var("RW_MCP_ALLOWED_WRITE") {
        let _ = std::fs::write(path, "allowed");
    }
    if let (Ok(denied), Ok(result)) = (
        std::env::var("RW_MCP_DENIED_WRITE"),
        std::env::var("RW_MCP_FILESYSTEM_RESULT"),
    ) {
        let outcome = if std::fs::write(denied, "forbidden").is_err() {
            "denied"
        } else {
            "unexpectedly_allowed"
        };
        let _ = std::fs::write(result, outcome);
    }
    if let (Ok(address), Ok(result)) = (
        std::env::var("RW_MCP_NETWORK_PROBE"),
        std::env::var("RW_MCP_NETWORK_RESULT"),
    ) {
        let outcome = address
            .parse()
            .ok()
            .and_then(|address| {
                std::net::TcpStream::connect_timeout(
                    &address,
                    std::time::Duration::from_millis(250),
                )
                .ok()
            })
            .map_or("denied", |_| "unexpectedly_allowed");
        let _ = std::fs::write(result, outcome);
    }
}
