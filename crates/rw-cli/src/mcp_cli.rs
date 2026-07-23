//! Administrative MCP commands. OAuth credential material terminates in rw-core.

use std::{fs, io::Write as _};

use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{McpOAuthLoginConfig, begin_mcp_oauth_login};
use rw_mcp::ServerId;
use rw_store::credentials::CredentialReference;
use url::Url;

use rw_runtime::executable_config::{DiscoveredMcpServer, DiscoveredMcpTransport};

pub(crate) async fn login(server_name: &str, dangerously_trust: bool) -> Result<()> {
    let workspace =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
    let credentials_path = loader.credentials_path();
    let loader = if dangerously_trust {
        loader.dangerously_trust_project()
    } else {
        loader
    };
    let effective = loader.load().into_diagnostic()?;
    let (user_home, _) = rw_runtime::session::extension_user_roots(&credentials_path);
    let catalog = rw_runtime::executable_config::discover_executable_configs(
        &user_home,
        &workspace,
        effective.project_trusted(),
    )?;
    for warning in &catalog.warnings {
        eprintln!("warning: {warning}");
    }
    let server = catalog
        .mcp_servers
        .iter()
        .find(|server| server.name == server_name)
        .ok_or_else(|| miette!("configured MCP server {server_name:?} was not found"))?;
    let config = login_configuration(server, credentials_path)?;
    let login = begin_mcp_oauth_login(config).await.into_diagnostic()?;
    println!("{}", login.authorization_url());
    std::io::stdout().flush().into_diagnostic()?;
    eprintln!(
        "waiting for MCP OAuth callback on {}; press Ctrl-C to cancel",
        login.redirect_uri()
    );
    let result = tokio::select! {
        result = login.complete() => result.into_diagnostic()?,
        signal = tokio::signal::ctrl_c() => {
            signal.into_diagnostic()?;
            return Err(miette!("MCP OAuth login cancelled"));
        }
    };
    for warning in result.warnings {
        eprintln!("warning: {warning}");
    }
    println!(
        "authenticated MCP server {}; restart active sessions to use the credential",
        result.server
    );
    Ok(())
}

fn login_configuration(
    server: &DiscoveredMcpServer,
    credentials_path: std::path::PathBuf,
) -> Result<McpOAuthLoginConfig> {
    let DiscoveredMcpTransport::Http {
        endpoint: _,
        oauth_credential: Some(credential_reference),
        oauth_resource: Some(resource),
        oauth_audience: Some(audience),
        oauth_authorization_endpoint: Some(authorization_endpoint),
        oauth_token_endpoint: Some(token_endpoint),
        oauth_client_id: Some(client_id),
        oauth_scopes,
        oauth_proxy,
    } = &server.transport
    else {
        return Err(miette!(
            "MCP server {:?} does not have a complete OAuth login configuration",
            server.name
        ));
    };
    Ok(McpOAuthLoginConfig {
        server: ServerId::new(server.name.clone()).map_err(|error| miette!(error.to_string()))?,
        authorization_endpoint: Url::parse(authorization_endpoint).into_diagnostic()?,
        token_endpoint: Url::parse(token_endpoint).into_diagnostic()?,
        client_id: client_id.clone(),
        scopes: oauth_scopes.clone(),
        proxy: oauth_proxy
            .as_deref()
            .map(Url::parse)
            .transpose()
            .into_diagnostic()?,
        credential_reference: CredentialReference::new(credential_reference),
        resource: resource.clone(),
        audience: audience.clone(),
        credentials_path,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use rw_runtime::executable_config::ExecutableConfigOrigin;

    #[test]
    fn login_requires_a_complete_http_oauth_profile() {
        let server = DiscoveredMcpServer {
            name: "remote".to_owned(),
            enabled: true,
            defer_tools: true,
            transport: DiscoveredMcpTransport::Http {
                endpoint: "https://mcp.example/mcp".to_owned(),
                oauth_credential: Some("mcp.remote.oauth".to_owned()),
                oauth_resource: Some("https://mcp.example/mcp".to_owned()),
                oauth_audience: Some("mcp.example".to_owned()),
                oauth_authorization_endpoint: None,
                oauth_token_endpoint: None,
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
                oauth_proxy: None,
            },
            credentials: Vec::new(),
            attested_files: Vec::new(),
            origin: ExecutableConfigOrigin::User(PathBuf::from("mcp.toml")),
            tool_capabilities: rw_mcp::McpToolCapabilityOverrides::default(),
            capability_override_origin: None,
        };
        assert!(login_configuration(&server, PathBuf::from("credentials.toml")).is_err());
    }
}
