//! Trusted, typed discovery for executable MCP and plugin configuration.

use std::{
    collections::BTreeMap,
    fmt, fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use miette::{IntoDiagnostic, Result, miette};
use rw_mcp::{McpServerConfig, McpTransportConfig};
use rw_plugin_protocol::PluginManifest;
use rw_tools::CapabilityManifest;
use rw_types::{McpServerId, ToolCapability};
use serde::Deserialize;
use serde::Serialize;
use url::Url;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_SERVERS: usize = 128;
const MAX_PLUGINS: usize = 128;
const MAX_ARG_BYTES: usize = 16 * 1024;
const MAX_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 16 * 1024;
const OAUTH_RESERVED_QUERY_PARAMETERS: [&str; 9] = [
    "response_type",
    "client_id",
    "redirect_uri",
    "state",
    "code_challenge",
    "code_challenge_method",
    "scope",
    "resource",
    "audience",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutableConfigOrigin {
    User(PathBuf),
    TrustedProject(PathBuf),
}

impl ExecutableConfigOrigin {
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::User(path) | Self::TrustedProject(path) => path,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialBinding {
    pub environment: String,
    pub credential_reference: String,
}

#[derive(Clone, Debug)]
pub struct DiscoveredMcpServer {
    pub name: String,
    pub enabled: bool,
    pub defer_tools: bool,
    pub transport: DiscoveredMcpTransport,
    pub credentials: Vec<CredentialBinding>,
    pub attested_files: Vec<ContentAttestation>,
    pub origin: ExecutableConfigOrigin,
    pub tool_capabilities: rw_mcp::McpToolCapabilityOverrides,
    pub capability_override_origin: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContentAttestation {
    pub path: PathBuf,
    pub length: u64,
    pub content_blake3: String,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl ContentAttestation {
    fn capture(path: &Path, max_bytes: u64) -> Result<Self> {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(miette!("attested command input cannot be a symlink"));
        }
        let path = fs::canonicalize(path).into_diagnostic()?;
        let metadata = fs::metadata(&path).into_diagnostic()?;
        if !metadata.is_file() {
            return Err(miette!("attested command input must be a regular file"));
        }
        if metadata.len() > max_bytes {
            return Err(miette!(
                "attested command input exceeds its remaining byte budget"
            ));
        }
        let file = fs::File::open(&path).into_diagnostic()?;
        let mut file = file.take(metadata.len().saturating_add(1));
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 16 * 1024];
        let mut read_bytes = 0_u64;
        loop {
            let count = file.read(&mut buffer).into_diagnostic()?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            read_bytes = read_bytes.saturating_add(count as u64);
        }
        if read_bytes != metadata.len() {
            return Err(miette!("attested command input changed while hashing"));
        }
        Ok(Self {
            path,
            length: metadata.len(),
            content_blake3: hasher.finalize().to_hex().to_string(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if &Self::capture(&self.path, self.length)? != self {
            return Err(miette!("approved command content identity changed"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub enum DiscoveredMcpTransport {
    Stdio {
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        inherit_env: Vec<String>,
        environment: Vec<(String, String)>,
        read_roots: Vec<PathBuf>,
        write_roots: Vec<PathBuf>,
        allowed_domains: Vec<String>,
    },
    Http {
        endpoint: String,
        oauth_credential: Option<String>,
        oauth_resource: Option<String>,
        oauth_audience: Option<String>,
        oauth_authorization_endpoint: Option<String>,
        oauth_token_endpoint: Option<String>,
        oauth_client_id: Option<String>,
        oauth_scopes: Vec<String>,
        oauth_proxy: Option<String>,
    },
}

impl fmt::Debug for DiscoveredMcpTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                argv,
                cwd,
                inherit_env,
                environment,
                read_roots,
                write_roots,
                allowed_domains,
            } => formatter
                .debug_struct("Stdio")
                .field("argv", argv)
                .field("cwd", cwd)
                .field("inherit_env", inherit_env)
                .field(
                    "environment_keys",
                    &environment.iter().map(|(key, _)| key).collect::<Vec<_>>(),
                )
                .field("read_roots", read_roots)
                .field("write_roots", write_roots)
                .field("allowed_domains", allowed_domains)
                .finish(),
            Self::Http {
                endpoint,
                oauth_credential,
                oauth_resource,
                oauth_audience,
                oauth_authorization_endpoint,
                oauth_token_endpoint,
                oauth_client_id,
                oauth_scopes,
                oauth_proxy,
            } => formatter
                .debug_struct("Http")
                .field("endpoint", endpoint)
                .field("oauth_credential", oauth_credential)
                .field("oauth_resource", oauth_resource)
                .field("oauth_audience", oauth_audience)
                .field("oauth_authorization_endpoint", oauth_authorization_endpoint)
                .field("oauth_token_endpoint", oauth_token_endpoint)
                .field("oauth_client_id", oauth_client_id)
                .field("oauth_scopes", oauth_scopes)
                .field("oauth_proxy", oauth_proxy)
                .finish(),
        }
    }
}

impl DiscoveredMcpServer {
    pub(crate) fn approval_fingerprint(&self) -> Result<String> {
        let transport = match &self.transport {
            DiscoveredMcpTransport::Stdio {
                argv,
                cwd,
                inherit_env,
                environment,
                read_roots,
                write_roots,
                allowed_domains,
            } => serde_json::json!({
                "kind":"stdio", "argv":argv, "cwd":cwd, "inherit_env":inherit_env,
                "environment":environment,
                "read_roots":read_roots, "write_roots":write_roots,
                "allowed_domains":allowed_domains,
                "credentials": self.credentials.iter().map(|binding| (&binding.environment, &binding.credential_reference)).collect::<Vec<_>>(),
                "attested_files":self.attested_files,
            }),
            DiscoveredMcpTransport::Http {
                endpoint,
                oauth_credential,
                oauth_resource,
                oauth_audience,
                oauth_authorization_endpoint,
                oauth_token_endpoint,
                oauth_client_id,
                oauth_scopes,
                oauth_proxy,
            } => serde_json::json!({
                "kind":"http", "endpoint":endpoint, "oauth_credential":oauth_credential,
                "oauth_resource":oauth_resource, "oauth_audience":oauth_audience,
                "oauth_authorization_endpoint":oauth_authorization_endpoint,
                "oauth_token_endpoint":oauth_token_endpoint, "oauth_client_id":oauth_client_id,
                "oauth_scopes":oauth_scopes, "oauth_proxy":oauth_proxy,
            }),
        };
        let bytes = serde_json::to_vec(&serde_json::json!({
            "name":self.name, "defer_tools":self.defer_tools,
            "origin":self.origin.path(), "transport":transport,
            "tool_capabilities":capability_override_json(&self.tool_capabilities),
            "capability_override_origin":self.capability_override_origin,
        }))
        .into_diagnostic()?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    pub(crate) fn runtime_config(
        &self,
        resolve: impl Fn(&str) -> Result<String>,
    ) -> Result<McpServerConfig> {
        let transport = match &self.transport {
            DiscoveredMcpTransport::Stdio {
                argv,
                cwd,
                inherit_env,
                environment,
                read_roots,
                write_roots,
                allowed_domains,
            } => {
                let mut environment = environment.clone();
                for name in inherit_env {
                    if let Ok(value) = std::env::var(name) {
                        environment.push((name.clone(), value));
                    }
                }
                for binding in &self.credentials {
                    environment.push((
                        binding.environment.clone(),
                        resolve(&binding.credential_reference)?,
                    ));
                }
                McpTransportConfig::Stdio {
                    executable: PathBuf::from(&argv[0]),
                    args: argv[1..].to_vec(),
                    working_directory: cwd.clone(),
                    environment,
                    sandbox: rw_mcp::McpStdioSandboxPolicy {
                        read_roots: read_roots.clone(),
                        write_roots: write_roots.clone(),
                        allowed_domains: allowed_domains.clone(),
                    },
                }
            }
            DiscoveredMcpTransport::Http {
                endpoint,
                oauth_credential,
                ..
            } => McpTransportConfig::StreamableHttp {
                endpoint: endpoint.clone(),
                oauth: oauth_credential.is_some(),
            },
        };
        Ok(McpServerConfig {
            id: McpServerId::new(self.name.clone()).map_err(|error| miette!(error.to_string()))?,
            transport,
            enabled: self.enabled,
            defer_tools: self.defer_tools,
            tool_capabilities: self.tool_capabilities.clone(),
        })
    }

    pub(crate) fn oauth_binding(&self) -> Option<(McpServerId, rw_core::McpOAuthBinding)> {
        let DiscoveredMcpTransport::Http {
            oauth_credential: Some(reference),
            oauth_resource: Some(resource),
            oauth_audience: Some(audience),
            oauth_token_endpoint,
            oauth_client_id,
            oauth_scopes,
            oauth_proxy,
            ..
        } = &self.transport
        else {
            return None;
        };
        let refresh = match (oauth_token_endpoint, oauth_client_id) {
            (Some(token_endpoint), Some(client_id)) => Some(rw_core::McpOAuthRefreshBinding {
                token_endpoint: Url::parse(token_endpoint).ok()?,
                client_id: client_id.clone(),
                scopes: oauth_scopes.clone(),
                proxy: oauth_proxy.as_deref().map(Url::parse).transpose().ok()?,
            }),
            (None, None) => None,
            _ => return None,
        };
        Some((
            McpServerId::new(self.name.clone()).ok()?,
            rw_core::McpOAuthBinding {
                token_reference: rw_store::credentials::CredentialReference::new(reference),
                resource: resource.clone(),
                audience: audience.clone(),
                refresh,
            },
        ))
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredPlugin {
    pub name: String,
    pub enabled: bool,
    pub target: DiscoveredPluginTarget,
    pub inherit_env: Vec<String>,
    pub manifest_path: PathBuf,
    pub allowed_domains: Vec<String>,
    pub origin: ExecutableConfigOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveredPluginTarget {
    Executable {
        argv: Vec<String>,
        cwd: PathBuf,
    },
    TypeScript {
        package_root: PathBuf,
        entry: PathBuf,
    },
}

impl DiscoveredPlugin {
    /// # Errors
    /// Returns an error when the discovered process authority is invalid.
    pub fn executable_process_config(&self) -> Result<rw_ext::PluginProcessConfig> {
        let DiscoveredPluginTarget::Executable { argv, cwd } = &self.target else {
            return Err(miette!(
                "TypeScript source plugins must be prepared by the source resolver"
            ));
        };
        rw_ext::PluginProcessConfig::new(PathBuf::from(&argv[0]))
            .and_then(|config| config.with_argv(argv[1..].iter().cloned()))
            .and_then(|config| config.with_cwd(cwd))
            .and_then(|config| {
                config.with_code_root(
                    self.manifest_path
                        .parent()
                        .ok_or(rw_ext::PluginProcessConfigError::InvalidAttestedFile)?,
                )
            })
            .and_then(|config| {
                config.with_attested_files(
                    self.attested_files()
                        .map_err(|_| rw_ext::PluginProcessConfigError::InvalidAttestedFile)?,
                )
            })
            .and_then(|config| config.with_environment_allowlist(self.inherit_env.iter().cloned()))
            .and_then(|config| config.with_allowed_domains(self.allowed_domains.iter().cloned()))
            .map_err(|error| miette!(error.to_string()))
    }

    fn attested_files(&self) -> Result<Vec<PathBuf>> {
        let code_root = self
            .manifest_path
            .parent()
            .ok_or_else(|| miette!("plugin manifest has no package directory"))?;
        let DiscoveredPluginTarget::Executable { argv, cwd } = &self.target else {
            return Ok(Vec::new());
        };
        Ok(attested_command_paths(argv, cwd)?
            .into_iter()
            .filter(|path| path == Path::new(&argv[0]) || path.starts_with(code_root))
            .collect())
    }

    /// # Errors
    /// Returns an error when the manifest is unreadable, invalid, or names another plugin.
    pub fn load_manifest(&self) -> Result<PluginManifest> {
        let bytes = read_private_config(&self.manifest_path)?;
        let manifest =
            PluginManifest::from_slice(&bytes).map_err(|error| miette!(error.to_string()))?;
        if manifest.name != self.name {
            return Err(miette!(
                "plugin {:?} manifest name {:?} does not match configuration",
                self.name,
                manifest.name
            ));
        }
        Ok(manifest)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExecutableConfigCatalog {
    pub mcp_servers: Vec<DiscoveredMcpServer>,
    pub plugins: Vec<DiscoveredPlugin>,
    pub warnings: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct McpFile {
    #[serde(default)]
    servers: BTreeMap<String, McpEntry>,
    #[serde(default)]
    capability_overrides: BTreeMap<String, McpCapabilityOverrideEntry>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct McpCapabilityOverrideEntry {
    #[serde(default)]
    default: Option<Vec<McpCapabilityName>>,
    #[serde(default)]
    tools: BTreeMap<String, Vec<McpCapabilityName>>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum McpCapabilityName {
    ReadsFs,
    WritesFs,
    Network,
    Exec,
}

impl From<McpCapabilityName> for ToolCapability {
    fn from(value: McpCapabilityName) -> Self {
        match value {
            McpCapabilityName::ReadsFs => Self::ReadFilesystem,
            McpCapabilityName::WritesFs => Self::WriteFilesystem,
            McpCapabilityName::Network => Self::Network,
            McpCapabilityName::Exec => Self::Execute,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpEntry {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default = "yes")]
    defer_tools: bool,
    argv: Option<Vec<String>>,
    endpoint: Option<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    inherit_env: Vec<String>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    credentials: BTreeMap<String, String>,
    #[serde(default)]
    read_roots: Vec<PathBuf>,
    #[serde(default)]
    write_roots: Vec<PathBuf>,
    #[serde(default)]
    allowed_domains: Vec<String>,
    oauth_credential: Option<String>,
    oauth_resource: Option<String>,
    oauth_audience: Option<String>,
    oauth_authorization_endpoint: Option<String>,
    oauth_token_endpoint: Option<String>,
    oauth_client_id: Option<String>,
    #[serde(default)]
    oauth_scopes: Vec<String>,
    oauth_proxy: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PluginsFile {
    #[serde(default)]
    plugins: Vec<PluginEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginEntry {
    name: String,
    #[serde(default = "yes")]
    enabled: bool,
    argv: Option<Vec<String>>,
    source: Option<PathBuf>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    inherit_env: Vec<String>,
    manifest: Option<PathBuf>,
    #[serde(default)]
    allowed_domains: Vec<String>,
}

const fn yes() -> bool {
    true
}

/// Discovers user and trusted-project MCP and plugin configuration.
///
/// # Errors
/// Returns an error when a layer is unsafe, malformed, or exceeds its bounds.
pub fn discover_executable_configs(
    user_home: &Path,
    project_root: &Path,
    project_trusted: bool,
) -> Result<ExecutableConfigCatalog> {
    let user_home = canonical_directory(user_home, "user home")?;
    let project_root = canonical_directory(project_root, "project root")?;
    let mut catalog = ExecutableConfigCatalog::default();
    let mut mcps = BTreeMap::new();
    let mut plugins = BTreeMap::new();

    let project_layers = [
        project_root.join(".agents"),
        project_root.join(".rottweiler"),
    ];
    let user_layers = [user_home.join(".agents"), user_home.join(".rottweiler")];
    if !project_trusted {
        for layer in &project_layers {
            for name in ["mcp.toml", "plugins.toml"] {
                let path = layer.join(name);
                if path.exists() {
                    catalog.warnings.push(format!(
                        "ignored untrusted executable project configuration {}",
                        path.display()
                    ));
                }
            }
        }
    }
    if project_trusted {
        for layer in &project_layers {
            load_mcp_layer(
                &layer.join("mcp.toml"),
                &ExecutableConfigOrigin::TrustedProject(layer.join("mcp.toml")),
                &project_root,
                &mut mcps,
            )?;
            load_plugin_layer(
                &layer.join("plugins.toml"),
                &ExecutableConfigOrigin::TrustedProject(layer.join("plugins.toml")),
                &project_root,
                &mut plugins,
            )?;
        }
    }
    for layer in &user_layers {
        load_mcp_layer(
            &layer.join("mcp.toml"),
            &ExecutableConfigOrigin::User(layer.join("mcp.toml")),
            &user_home,
            &mut mcps,
        )?;
        load_plugin_layer(
            &layer.join("plugins.toml"),
            &ExecutableConfigOrigin::User(layer.join("plugins.toml")),
            &user_home,
            &mut plugins,
        )?;
    }
    catalog.mcp_servers = mcps.into_values().collect();
    catalog.plugins = plugins.into_values().collect();
    Ok(catalog)
}

pub(crate) fn discover_tui_stdio_server(
    path: &Path,
    base: &Path,
    name: &str,
    executable: &Path,
    args: Vec<String>,
    environment: Vec<(String, String)>,
) -> Result<DiscoveredMcpServer> {
    let mut command_values = Vec::with_capacity(args.len() + 1);
    command_values.push(executable.to_string_lossy().into_owned());
    command_values.extend(args);
    let environment = environment.into_iter().collect::<BTreeMap<_, _>>();
    parse_mcp_server(
        path,
        &ExecutableConfigOrigin::User(path.to_owned()),
        base,
        name.to_owned(),
        McpEntry {
            enabled: false,
            defer_tools: true,
            argv: Some(command_values),
            endpoint: None,
            cwd: None,
            inherit_env: Vec::new(),
            environment,
            credentials: BTreeMap::new(),
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            allowed_domains: Vec::new(),
            oauth_credential: None,
            oauth_resource: None,
            oauth_audience: None,
            oauth_authorization_endpoint: None,
            oauth_token_endpoint: None,
            oauth_client_id: None,
            oauth_scopes: Vec::new(),
            oauth_proxy: None,
        },
    )
}

fn load_mcp_layer(
    path: &Path,
    origin: &ExecutableConfigOrigin,
    base: &Path,
    out: &mut BTreeMap<String, DiscoveredMcpServer>,
) -> Result<()> {
    let Some(bytes) = read_optional_config(path)? else {
        return Ok(());
    };
    let file: McpFile =
        toml::from_slice(&bytes).map_err(|error| miette!("{}: {error}", path.display()))?;
    if file.servers.len() > MAX_SERVERS {
        return Err(miette!("{} contains too many MCP servers", path.display()));
    }
    if matches!(origin, ExecutableConfigOrigin::TrustedProject(_))
        && !file.capability_overrides.is_empty()
    {
        return Err(miette!(
            "{}: MCP capability_overrides are user-scoped security configuration",
            path.display()
        ));
    }
    for (name, entry) in file.servers {
        let server = parse_mcp_server(path, origin, base, name.clone(), entry)?;
        out.entry(name).or_insert(server);
    }
    for (server, entry) in file.capability_overrides {
        let configured = out.get_mut(&server).ok_or_else(|| {
            miette!(
                "{}: capability override references unknown MCP server {server}",
                path.display()
            )
        })?;
        if configured.capability_override_origin.is_none() {
            configured.tool_capabilities = parse_capability_override(entry)?;
            configured.capability_override_origin = Some(path.to_path_buf());
        }
    }
    Ok(())
}

fn parse_mcp_server(
    path: &Path,
    origin: &ExecutableConfigOrigin,
    base: &Path,
    name: String,
    mut entry: McpEntry,
) -> Result<DiscoveredMcpServer> {
    let id =
        McpServerId::new(name.clone()).map_err(|error| miette!("{}: {error}", path.display()))?;
    validate_env_names(&entry.inherit_env, false)?;
    validate_literal_environment(&entry.environment)?;
    let credentials = parse_credential_bindings(std::mem::take(&mut entry.credentials))?;
    if entry.environment.keys().any(|name| {
        entry.inherit_env.contains(name)
            || credentials
                .iter()
                .any(|binding| binding.environment == *name)
    }) {
        return Err(miette!(
            "MCP environment names must not overlap inherited or credential-backed names"
        ));
    }
    let argv = entry
        .argv
        .take()
        .map(|argv| pin_argv(argv, base))
        .transpose()?;
    let endpoint = entry.endpoint.take();
    let enabled = entry.enabled;
    let defer_tools = entry.defer_tools;
    let transport = match (argv, endpoint) {
        (Some(argv), None) => parse_stdio_transport(&id, base, argv, entry)?,
        (None, Some(endpoint)) => parse_http_transport(&id, endpoint, entry, &credentials)?,
        _ => {
            return Err(miette!(
                "MCP server {id} must declare exactly one of argv or endpoint"
            ));
        }
    };
    let attested_files = match &transport {
        DiscoveredMcpTransport::Stdio { argv, cwd, .. } => {
            capture_command_attestations(argv, cwd.as_deref().unwrap_or(base))?
        }
        DiscoveredMcpTransport::Http { .. } => Vec::new(),
    };
    Ok(DiscoveredMcpServer {
        name,
        enabled,
        defer_tools,
        transport,
        credentials,
        attested_files,
        origin: origin.clone(),
        tool_capabilities: rw_mcp::McpToolCapabilityOverrides::default(),
        capability_override_origin: None,
    })
}

fn parse_capability_override(
    entry: McpCapabilityOverrideEntry,
) -> Result<rw_mcp::McpToolCapabilityOverrides> {
    if entry.tools.len() > 128 {
        return Err(miette!("MCP tool capability override exceeds 128 tools"));
    }
    let server_default = entry.default.map(capability_manifest).transpose()?;
    let tools = entry
        .tools
        .into_iter()
        .map(|(name, capabilities)| {
            validate_mcp_tool_name(&name)?;
            Ok((name, capability_manifest(capabilities)?))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(rw_mcp::McpToolCapabilityOverrides {
        server_default,
        tools,
    })
}

fn capability_manifest(values: Vec<McpCapabilityName>) -> Result<CapabilityManifest> {
    if values.len() > 4 {
        return Err(miette!("MCP capability list exceeds four classes"));
    }
    let capabilities = values
        .into_iter()
        .map(ToolCapability::from)
        .collect::<Vec<_>>();
    if CapabilityManifest::new(capabilities.clone())
        .capabilities()
        .len()
        != capabilities.len()
    {
        return Err(miette!("MCP capability list contains a duplicate class"));
    }
    Ok(CapabilityManifest::new(capabilities))
}

fn validate_mcp_tool_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 256
        || name
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(miette!(
            "MCP capability override tool name is empty, oversized, or contains whitespace"
        ));
    }
    Ok(())
}

pub(crate) fn capability_override_json(
    overrides: &rw_mcp::McpToolCapabilityOverrides,
) -> serde_json::Value {
    let encode = |manifest: &CapabilityManifest| manifest.capabilities().to_vec();
    serde_json::json!({
        "default":overrides.server_default.as_ref().map(&encode),
        "tools":overrides.tools.iter().map(|(name, manifest)| (name, encode(manifest))).collect::<BTreeMap<_,_>>(),
    })
}

fn parse_credential_bindings(
    credentials: BTreeMap<String, String>,
) -> Result<Vec<CredentialBinding>> {
    credentials
        .into_iter()
        .map(|(environment, credential_reference)| {
            validate_env_name(&environment, true)?;
            validate_reference(&credential_reference)?;
            Ok(CredentialBinding {
                environment,
                credential_reference,
            })
        })
        .collect()
}

fn parse_stdio_transport(
    id: &McpServerId,
    base: &Path,
    argv: Vec<String>,
    entry: McpEntry,
) -> Result<DiscoveredMcpTransport> {
    if entry.oauth_credential.is_some() {
        return Err(miette!(
            "MCP stdio server {id} cannot declare oauth_credential"
        ));
    }
    let cwd = entry
        .cwd
        .map(|cwd| resolve_existing_directory(base, &cwd))
        .transpose()?;
    let read_roots = resolve_mcp_roots(base, &entry.read_roots, "read_roots")?;
    let write_roots = resolve_mcp_roots(base, &entry.write_roots, "write_roots")?;
    let allowed_domains = validate_domains(entry.allowed_domains, "MCP stdio server")?;
    Ok(DiscoveredMcpTransport::Stdio {
        argv,
        cwd,
        inherit_env: entry.inherit_env,
        environment: entry.environment.into_iter().collect(),
        read_roots,
        write_roots,
        allowed_domains,
    })
}

fn resolve_mcp_roots(base: &Path, roots: &[PathBuf], field: &str) -> Result<Vec<PathBuf>> {
    if roots.len() > 64 {
        return Err(miette!("MCP stdio {field} exceeds 64 entries"));
    }
    let mut roots = roots
        .iter()
        .map(|root| resolve_existing_directory(base, root))
        .collect::<Result<Vec<_>>>()?;
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn parse_http_transport(
    id: &McpServerId,
    endpoint: String,
    entry: McpEntry,
    credentials: &[CredentialBinding],
) -> Result<DiscoveredMcpTransport> {
    if entry.cwd.is_some()
        || !entry.inherit_env.is_empty()
        || !entry.environment.is_empty()
        || !credentials.is_empty()
        || !entry.read_roots.is_empty()
        || !entry.write_roots.is_empty()
        || !entry.allowed_domains.is_empty()
    {
        return Err(miette!(
            "MCP HTTP server {id} cannot declare stdio environment, cwd, or sandbox authority"
        ));
    }
    validate_endpoint(&endpoint)?;
    validate_http_oauth_binding(&endpoint, &entry)?;
    let oauth_login_configured = validate_oauth_login_configuration(
        entry.oauth_authorization_endpoint.as_deref(),
        entry.oauth_token_endpoint.as_deref(),
        entry.oauth_client_id.as_deref(),
        &entry.oauth_scopes,
        entry.oauth_proxy.as_deref(),
    )?;
    if oauth_login_configured && entry.oauth_credential.is_none() {
        return Err(miette!(
            "MCP HTTP OAuth login configuration requires the bound oauth_credential, oauth_resource, and oauth_audience fields"
        ));
    }
    Ok(DiscoveredMcpTransport::Http {
        endpoint,
        oauth_credential: entry.oauth_credential,
        oauth_resource: entry.oauth_resource,
        oauth_audience: entry.oauth_audience,
        oauth_authorization_endpoint: entry.oauth_authorization_endpoint,
        oauth_token_endpoint: entry.oauth_token_endpoint,
        oauth_client_id: entry.oauth_client_id,
        oauth_scopes: entry.oauth_scopes,
        oauth_proxy: entry.oauth_proxy,
    })
}

fn validate_http_oauth_binding(endpoint: &str, entry: &McpEntry) -> Result<()> {
    match (
        &entry.oauth_credential,
        &entry.oauth_resource,
        &entry.oauth_audience,
    ) {
        (None, None, None) => Ok(()),
        (Some(reference), Some(resource), Some(audience)) => {
            validate_reference(reference)?;
            if rw_store::credentials::CredentialReference::new(reference)
                .environment_variable()
                .is_some()
            {
                return Err(miette!(
                    "MCP OAuth credentials must use the Rottweiler credential vault, not an environment reference"
                ));
            }
            validate_oauth_binding(resource, audience)?;
            if resource != endpoint {
                return Err(miette!(
                    "MCP OAuth resource must exactly match the configured MCP endpoint"
                ));
            }
            Ok(())
        }
        _ => Err(miette!(
            "MCP HTTP OAuth requires oauth_credential, oauth_resource, and oauth_audience together"
        )),
    }
}

fn load_plugin_layer(
    path: &Path,
    origin: &ExecutableConfigOrigin,
    base: &Path,
    out: &mut BTreeMap<String, DiscoveredPlugin>,
) -> Result<()> {
    let Some(bytes) = read_optional_config(path)? else {
        return Ok(());
    };
    let file: PluginsFile =
        toml::from_slice(&bytes).map_err(|error| miette!("{}: {error}", path.display()))?;
    if file.plugins.len() > MAX_PLUGINS {
        return Err(miette!("{} contains too many plugins", path.display()));
    }
    for entry in file.plugins {
        validate_plugin_name(&entry.name)?;
        validate_env_names(&entry.inherit_env, false)?;
        let (target, manifest_path) = match (entry.argv, entry.source, entry.manifest, entry.cwd) {
            (Some(argv), None, Some(manifest), cwd) => {
                let argv = pin_argv(argv, base)?;
                let manifest_path = resolve_existing_file(base, &manifest)?;
                let cwd = cwd.as_ref().map_or_else(
                    || {
                        manifest_path
                            .parent()
                            .map(Path::to_path_buf)
                            .ok_or_else(|| miette!("plugin manifest has no package directory"))
                    },
                    |cwd| resolve_existing_directory(base, cwd),
                )?;
                (
                    DiscoveredPluginTarget::Executable { argv, cwd },
                    manifest_path,
                )
            }
            (None, Some(source), None, None) => {
                let package_root = resolve_existing_directory(base, &source)?;
                let manifest_path =
                    resolve_existing_file(&package_root, Path::new("manifest.json"))?;
                let entry = resolve_existing_file(&package_root, Path::new("src/index.ts"))?;
                (
                    DiscoveredPluginTarget::TypeScript {
                        package_root,
                        entry,
                    },
                    manifest_path,
                )
            }
            _ => {
                return Err(miette!(
                    "plugin {:?} must declare exactly one target: argv with manifest, or source alone",
                    entry.name
                ));
            }
        };
        let allowed_domains = validate_domains(entry.allowed_domains, "plugin")?;
        out.entry(entry.name.clone()).or_insert(DiscoveredPlugin {
            name: entry.name,
            enabled: entry.enabled,
            target,
            inherit_env: entry.inherit_env,
            manifest_path,
            allowed_domains,
            origin: origin.clone(),
        });
    }
    Ok(())
}

fn read_optional_config(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(miette!("{} must be a real regular file", path.display()));
            }
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err(miette!(
                    "{} exceeds the configuration size cap",
                    path.display()
                ));
            }
            let file = fs::File::open(path).into_diagnostic()?;
            let capacity = usize::try_from(metadata.len())
                .map_err(|_| miette!("{} is too large for this platform", path.display()))?;
            let mut bytes = Vec::with_capacity(capacity);
            file.take(MAX_CONFIG_BYTES + 1)
                .read_to_end(&mut bytes)
                .into_diagnostic()?;
            if bytes.len() as u64 > MAX_CONFIG_BYTES {
                return Err(miette!(
                    "{} exceeds the configuration size cap",
                    path.display()
                ));
            }
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).into_diagnostic(),
    }
}

fn read_private_config(path: &Path) -> Result<Vec<u8>> {
    read_optional_config(path)?.ok_or_else(|| miette!("{} does not exist", path.display()))
}
fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let value = fs::canonicalize(path)
        .map_err(|error| miette!("{label} {} is unavailable: {error}", path.display()))?;
    if !value.is_dir() {
        return Err(miette!("{label} is not a directory"));
    }
    Ok(value)
}
fn resolve_existing_directory(base: &Path, value: &Path) -> Result<PathBuf> {
    let path = if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    };
    canonical_directory(&path, "configured working directory")
}
fn resolve_existing_file(base: &Path, value: &Path) -> Result<PathBuf> {
    let path = if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    };
    let path = fs::canonicalize(&path).into_diagnostic()?;
    if !path.is_file() {
        return Err(miette!("configured manifest is not a file"));
    }
    Ok(path)
}
fn validate_argv(argv: Option<&[String]>) -> Result<()> {
    let Some(argv) = argv else {
        return Ok(());
    };
    if argv.is_empty()
        || argv.len() > 256
        || argv
            .iter()
            .any(|arg| arg.is_empty() || arg.len() > MAX_ARG_BYTES || arg.as_bytes().contains(&0))
    {
        return Err(miette!(
            "argv must contain 1..256 bounded non-empty literal arguments"
        ));
    }
    Ok(())
}
fn pin_argv(mut argv: Vec<String>, base: &Path) -> Result<Vec<String>> {
    validate_argv(Some(&argv))?;
    let supplied = PathBuf::from(&argv[0]);
    if !supplied.is_absolute() {
        return Err(miette!(
            "configured executable must be an absolute path; PATH lookup is not permitted"
        ));
    }
    let executable = fs::canonicalize(if supplied.is_absolute() {
        supplied
    } else {
        base.join(supplied)
    })
    .into_diagnostic()?;
    let metadata = fs::metadata(&executable).into_diagnostic()?;
    if !metadata.is_file() {
        return Err(miette!(
            "configured executable must resolve to a regular file"
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(miette!("configured executable is not executable"));
        }
    }
    argv[0] = executable.to_string_lossy().into_owned();
    Ok(argv)
}

const ATTESTATION_FILES: [&str; 12] = [
    "package.json",
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "requirements.txt",
    "requirements.lock",
    "Cargo.lock",
];

fn attested_command_paths(argv: &[String], cwd: &Path) -> Result<Vec<PathBuf>> {
    const INTERPRETERS: [&str; 8] = [
        "bun", "node", "deno", "python", "python3", "ruby", "perl", "php",
    ];
    const AMBIGUOUS: [&str; 8] = ["-c", "-e", "--eval", "run", "x", "dlx", "exec", "-m"];
    const PACKAGE_RUNNERS: [&str; 12] = [
        "bunx", "corepack", "cargo", "go", "npm", "npx", "pipx", "pnpm", "pnpx", "uvx", "yarn",
        "yarnpkg",
    ];
    let executable = Path::new(&argv[0]);
    let basename = executable
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if PACKAGE_RUNNERS.contains(&basename.as_str()) {
        return Err(miette!(
            "production plugin/MCP commands cannot use package-runner executables"
        ));
    }
    let interpreted = INTERPRETERS.contains(&basename.as_str()) || basename.starts_with("python");
    if interpreted
        && argv[1..]
            .iter()
            .any(|argument| AMBIGUOUS.contains(&argument.as_str()))
    {
        return Err(miette!(
            "production plugin/MCP commands cannot use eval, module, or package-runner forms"
        ));
    }
    let mut paths = vec![executable.to_path_buf()];
    let mut argument_files = 0_usize;
    for argument in &argv[1..] {
        if argument.starts_with('-') {
            continue;
        }
        let supplied = Path::new(argument);
        let candidate = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            cwd.join(supplied)
        };
        if fs::symlink_metadata(&candidate).is_ok_and(|metadata| metadata.is_file()) {
            paths.push(candidate);
            argument_files += 1;
        }
    }
    if interpreted && argument_files == 0 {
        return Err(miette!(
            "production interpreter command requires an explicit regular-file entrypoint"
        ));
    }
    let entry_parent = paths
        .get(1)
        .and_then(|path| path.parent())
        .unwrap_or(cwd)
        .to_path_buf();
    for directory in [cwd.to_path_buf(), entry_parent] {
        for name in ATTESTATION_FILES {
            let path = directory.join(name);
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    if paths.len() > 64 {
        return Err(miette!("command content attestation exceeds 64 files"));
    }
    Ok(paths)
}

fn capture_command_attestations(argv: &[String], cwd: &Path) -> Result<Vec<ContentAttestation>> {
    let paths = attested_command_paths(argv, cwd)?;
    let mut total = 0_u64;
    let mut attestations = Vec::with_capacity(paths.len());
    for path in paths {
        let remaining = (256_u64 * 1024 * 1024).saturating_sub(total);
        let attestation = ContentAttestation::capture(&path, remaining)?;
        total = total
            .checked_add(attestation.length)
            .ok_or_else(|| miette!("command content attestation size overflowed"))?;
        if total > 256 * 1024 * 1024 {
            return Err(miette!("command content attestation exceeds 256 MiB"));
        }
        attestations.push(attestation);
    }
    Ok(attestations)
}
fn validate_plugin_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(miette!("invalid plugin name {name:?}"));
    }
    Ok(())
}
fn validate_reference(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_whitespace) {
        return Err(miette!(
            "credential references must be bounded, non-empty identifiers"
        ));
    }
    Ok(())
}
fn validate_oauth_binding(resource: &str, audience: &str) -> Result<()> {
    if resource.is_empty()
        || resource.len() > 2048
        || audience.is_empty()
        || audience.len() > 512
        || resource.chars().any(char::is_whitespace)
        || audience.chars().any(char::is_whitespace)
    {
        return Err(miette!("MCP OAuth resource/audience binding is invalid"));
    }
    let url =
        Url::parse(resource).map_err(|_| miette!("MCP OAuth resource must be an absolute URL"))?;
    if url.scheme() != "https"
        && !(url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }))
    {
        return Err(miette!(
            "MCP OAuth resource must use HTTPS (loopback HTTP is permitted)"
        ));
    }
    Ok(())
}

fn validate_oauth_login_configuration(
    authorization_endpoint: Option<&str>,
    token_endpoint: Option<&str>,
    client_id: Option<&str>,
    scopes: &[String],
    proxy: Option<&str>,
) -> Result<bool> {
    let configured = authorization_endpoint.is_some()
        || token_endpoint.is_some()
        || client_id.is_some()
        || !scopes.is_empty()
        || proxy.is_some();
    if !configured {
        return Ok(false);
    }
    let (Some(authorization_endpoint), Some(token_endpoint), Some(client_id)) =
        (authorization_endpoint, token_endpoint, client_id)
    else {
        return Err(miette!(
            "MCP OAuth login requires oauth_authorization_endpoint, oauth_token_endpoint, and oauth_client_id together"
        ));
    };
    validate_oauth_endpoint("authorization", authorization_endpoint)?;
    validate_oauth_endpoint("token", token_endpoint)?;
    if client_id.is_empty() || client_id.len() > 2048 || client_id.chars().any(char::is_control) {
        return Err(miette!("MCP OAuth client id is invalid"));
    }
    if scopes.len() > 64
        || scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > 512
                || scope
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        })
    {
        return Err(miette!("MCP OAuth scopes are invalid"));
    }
    if let Some(proxy) = proxy {
        validate_oauth_proxy(proxy)?;
    }
    Ok(true)
}

fn validate_oauth_endpoint(kind: &str, endpoint: &str) -> Result<()> {
    let url = Url::parse(endpoint)
        .map_err(|_| miette!("MCP OAuth {kind} endpoint is not an absolute URL"))?;
    validate_secure_url(&url, &format!("MCP OAuth {kind} endpoint"))?;
    if kind == "authorization"
        && url.query_pairs().any(|(name, _)| {
            OAUTH_RESERVED_QUERY_PARAMETERS
                .iter()
                .any(|reserved| name == *reserved)
        })
    {
        return Err(miette!(
            "MCP OAuth authorization endpoint cannot preconfigure protocol parameters"
        ));
    }
    Ok(())
}

fn validate_oauth_proxy(proxy: &str) -> Result<()> {
    let url = Url::parse(proxy).map_err(|_| miette!("MCP OAuth proxy is not an absolute URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(miette!(
            "MCP OAuth proxy must be an HTTP(S) URL without credentials, query, or fragment"
        ));
    }
    Ok(())
}

fn validate_secure_url(url: &Url, label: &str) -> Result<()> {
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
    {
        return Err(miette!(
            "{label} must use HTTPS without credentials or a fragment (loopback HTTP is permitted)"
        ));
    }
    Ok(())
}
fn validate_env_names(names: &[String], credential: bool) -> Result<()> {
    for name in names {
        validate_env_name(name, credential)?;
    }
    Ok(())
}
fn validate_literal_environment(environment: &BTreeMap<String, String>) -> Result<()> {
    if environment.len() > MAX_ENVIRONMENT_ENTRIES {
        return Err(miette!("MCP environment exceeds 256 entries"));
    }
    for (name, value) in environment {
        validate_env_name(name, true)?;
        if value.len() > MAX_ENVIRONMENT_VALUE_BYTES || value.as_bytes().contains(&0) {
            return Err(miette!("MCP environment value is invalid or oversized"));
        }
    }
    Ok(())
}
fn validate_env_name(name: &str, credential: bool) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(miette!("invalid environment name {name:?}"));
    }
    if !credential
        && !matches!(
            name,
            "PATH"
                | "HOME"
                | "TMPDIR"
                | "LANG"
                | "LC_ALL"
                | "LC_CTYPE"
                | "TERM"
                | "COLORTERM"
                | "NO_COLOR"
        )
    {
        return Err(miette!(
            "security-sensitive environment {name:?} requires a credential reference"
        ));
    }
    Ok(())
}
fn validate_endpoint(endpoint: &str) -> Result<()> {
    let url = Url::parse(endpoint).map_err(|error| miette!("invalid MCP endpoint: {error}"))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(miette!(
            "MCP endpoint cannot contain credentials or fragments"
        ));
    }
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(miette!(
            "MCP endpoint must use HTTPS (loopback HTTP is permitted)"
        ));
    }
    Ok(())
}

fn validate_domains(domains: Vec<String>, subject: &str) -> Result<Vec<String>> {
    if domains.len() > 32 {
        return Err(miette!("{subject} allowed_domains exceeds 32 entries"));
    }
    let mut normalized = domains
        .into_iter()
        .map(|domain| {
            let domain = rw_tools::normalize_egress_domain(&domain)
                .ok_or_else(|| miette!("invalid {subject} allowed domain"))?;
            let local_suffix = domain.rsplit_once('.').is_some_and(|(_, suffix)| {
                suffix.eq_ignore_ascii_case("localhost") || suffix.eq_ignore_ascii_case("local")
            });
            if domain.eq_ignore_ascii_case("localhost")
                || local_suffix
                || domain.parse::<std::net::IpAddr>().is_ok()
            {
                return Err(miette!(
                    "{subject} allowed_domains cannot include local or private destinations"
                ));
            }
            Ok(domain)
        })
        .collect::<Result<Vec<_>>>()?;
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
