use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use clap::{Parser, Subcommand, ValueEnum};
use miette::{IntoDiagnostic, Result, miette};
use rw_core::runtime_support::maybe_run_sandbox_helper;
use rw_core::{
    ClientCommand, ClientId, CommandOutcome, CreateSessionRequest, DEFAULT_MODEL_CATALOG_URL,
    EngineEvent, EngineHost, EngineHostConfig, GitHubCopilotLogin, OAuthLogin, ProviderApiKey,
    ProviderLogin, ProviderLoginCancellation, SequenceId, SessionId, begin_provider_login,
    refresh_model_catalog, store_provider_api_key,
};
use tracing_subscriber::EnvFilter;

mod doctor;
mod history;
#[allow(dead_code)]
mod host_runtime;
mod import;
#[allow(dead_code)]
mod m8_config;
#[allow(dead_code)]
mod m8_runtime;
mod mcp_cli;
mod mcp_server;
mod plugin_cli;
mod plugin_dev;
#[allow(dead_code)]
mod plugin_launcher;
mod project_commands;
#[allow(dead_code)]
mod remote;
mod runtime;
#[allow(dead_code)]
mod server;
#[allow(dead_code)]
mod shell_broker;
mod stats;
mod subagent_metadata;
#[allow(dead_code)]
mod supervisor;
#[allow(dead_code)]
mod tty;
mod tui_config;
mod upgrade;
mod workflow_runtime;

/// Normalizes rustix's platform-native device identifier without assuming the
/// signed/unsigned width selected by a particular Unix libc ABI.
#[cfg(unix)]
pub(crate) fn rustix_device_id<T: TryInto<u64>>(device: T) -> Option<u64> {
    device.try_into().ok()
}

/// Widens rustix's platform-native mode representation for stable bit tests.
#[cfg(unix)]
pub(crate) fn rustix_mode_bits<T: Into<u32>>(mode: T) -> u32 {
    mode.into()
}

#[derive(Debug, Parser)]
#[command(name = "rw", version, about = "Rottweiler coding-agent harness")]
#[allow(clippy::struct_excessive_bools)]
struct Cli {
    /// Run one prompt without starting the interactive line-mode client.
    #[arg(short = 'p', long, value_name = "PROMPT")]
    prompt: Option<String>,
    /// Rendering contract for print mode.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text, global = true)]
    output_format: OutputFormat,
    /// Non-interactive permission policy. Omitted means the loaded config policy.
    #[arg(long, value_enum, global = true)]
    permission_mode: Option<PermissionMode>,
    /// Maximum provider iterations permitted in one user turn.
    #[arg(long, default_value_t = 32, global = true)]
    max_turns: usize,
    /// Run the `OpenTUI` locally against an engine reached over SSH.
    #[arg(long, value_name = "HOST", global = true)]
    remote: Option<String>,
    /// Workspace path on the remote engine host; defaults to the local path.
    #[arg(long, value_name = "PATH", requires = "remote", global = true)]
    remote_workspace: Option<PathBuf>,
    /// Keep the engine alive after the interactive client exits.
    #[arg(long, global = true)]
    detach: bool,
    /// Use the pre-M4 readline client instead of `OpenTUI`.
    #[arg(long, global = true)]
    line: bool,
    /// Add another canonical workspace root for tools and sandbox writes.
    #[arg(long = "add-dir", value_name = "PATH", global = true)]
    add_dirs: Vec<PathBuf>,
    /// Enable executable project configuration without persisting trust.
    #[arg(long, global = true)]
    dangerously_trust: bool,
    /// Resume an exact durable session id.
    #[arg(
        long,
        value_name = "SESSION",
        conflicts_with = "continue_latest",
        global = true
    )]
    resume: Option<String>,
    /// Continue the most recently updated durable session.
    #[arg(long = "continue", conflicts_with = "resume", global = true)]
    continue_latest: bool,
    /// Network-free provider recording directory used by deterministic tests.
    #[arg(long, hide = true, value_name = "DIRECTORY")]
    replay_dir: Option<PathBuf>,
    /// Record a deterministic provider-event script for CLI acceptance tests.
    #[arg(long, hide = true, value_name = "SCRIPT", requires = "replay_dir")]
    record_replay_script: Option<PathBuf>,
    /// Use a deterministic in-memory provider-event script without fixture I/O.
    #[arg(
        long,
        hide = true,
        value_name = "SCRIPT",
        conflicts_with = "record_replay_script"
    )]
    in_memory_replay_script: Option<PathBuf>,
    /// Delay each scripted provider event for crash/interrupt acceptance tests.
    #[arg(long, hide = true, default_value_t = 0)]
    record_script_delay_ms: u64,
    /// Emit deterministic timing markers for the release performance smoke.
    #[arg(long, hide = true)]
    perf_markers: bool,
    /// Provider name stored in the deterministic replay directory.
    #[arg(long, hide = true, default_value = "cli-replay")]
    replay_provider: String,
    /// Override the active provider-neutral model alias.
    #[arg(long, value_name = "ALIAS", global = true)]
    model: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    #[default]
    Text,
    Json,
    StreamJson,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum PermissionMode {
    Strict,
    AutoSafe,
    Yolo,
}

impl PermissionMode {
    const fn as_cli_value(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::AutoSafe => "auto-safe",
            Self::Yolo => "yolo",
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Internal release-installer durability helper.
    #[command(name = "__install-sync", hide = true)]
    InstallSync {
        /// Exact regular files/directories to flush without following symlinks.
        #[arg(value_name = "PATH", num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Run the authenticated headless engine server.
    Serve {
        /// Unix socket path; defaults to `ROTTWEILER_ENGINE_SOCKET`.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        /// Private bootstrap-token path; defaults to `ROTTWEILER_ENGINE_TOKEN_FILE`.
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
        /// Initial durable session id; defaults to `ROTTWEILER_SESSION_ID`.
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
        /// Engine-host workspace; defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
        /// Wait for a crashed predecessor's watchdog to release workspace ownership.
        #[arg(long, hide = true)]
        wait_for_execution_lease: bool,
    },
    /// Inspect an assembled model prompt without calling a provider.
    Prompt {
        #[command(subcommand)]
        command: PromptCommand,
    },
    /// Inspect Rottweiler configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage the refreshable model capability and pricing table.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
    /// Authenticate a user-configured provider.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Inspect or change folder trust for the current workspace.
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },
    /// Author and debug out-of-process plugins.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Expose approved Rottweiler tools and connection-owned sessions over MCP.
    McpServer {
        #[command(subcommand)]
        command: McpServerCommand,
    },
    /// Manage configured MCP clients.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Replay a persisted event stream without opening an engine or provider.
    Replay {
        #[arg(value_name = "SESSION")]
        session: String,
        /// Emit machine-readable `EngineEvent` JSONL instead of launching the TUI.
        #[arg(long)]
        jsonl: bool,
    },
    /// Export one persisted transcript without opening credentials or providers.
    Export {
        #[arg(value_name = "SESSION")]
        session: String,
        #[arg(long, value_enum, default_value_t = HistoryExportFormat::Markdown)]
        format: HistoryExportFormat,
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Atomically replace an existing regular, single-link output file.
        #[arg(long, requires = "output")]
        force: bool,
    },
    /// Search durable session titles and transcripts.
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// Report bounded historical tokens, costs, cache savings, and tool use.
    Stats {
        /// Limit the report to this session and its durable subagent descendants.
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
        /// Inclusive UTC start day (`YYYY-MM-DD`).
        #[arg(long, value_name = "YYYY-MM-DD")]
        from: Option<String>,
        /// Inclusive UTC end day (`YYYY-MM-DD`).
        #[arg(long = "to", value_name = "YYYY-MM-DD")]
        through: Option<String>,
        /// Emit the stable JSON report (equivalent to `--output-format json`).
        #[arg(long)]
        json: bool,
    },
    /// Import declarative configuration without reading credentials or executing content.
    Import {
        #[arg(value_enum)]
        source: import::ImportSource,
        /// Source project/config directory.
        #[arg(long, value_name = "PATH")]
        source_root: PathBuf,
        /// Existing target project root; defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        target: Option<PathBuf>,
        /// Plan and report without creating files.
        #[arg(long)]
        dry_run: bool,
        /// Emit the stable JSON report.
        #[arg(long)]
        json: bool,
    },
    /// Diagnose configuration, credentials, sandbox, terminal, and providers.
    Doctor {
        /// Opt in to bounded provider reachability and credential-validation probes.
        #[arg(long)]
        network: bool,
        /// Per-provider connect and request timeout in milliseconds.
        #[arg(long, default_value_t = 3_000, value_name = "MILLISECONDS")]
        timeout_ms: u64,
        /// Emit the stable machine-readable diagnostic report.
        #[arg(long)]
        json: bool,
    },
    /// Install a signed stable/beta release or atomically select the previous generation.
    Upgrade {
        /// Override the effective user-scoped update channel for this invocation.
        #[arg(long, value_enum)]
        channel: Option<UpgradeChannel>,
        /// Permit a lower product version only when all signatures and rollback checks pass.
        #[arg(long, conflicts_with = "rollback")]
        allow_downgrade: bool,
        /// Atomically reactivate the previous locally verified generation without networking.
        #[arg(long, conflicts_with = "channel")]
        rollback: bool,
        /// Per-fetch connect and whole-response deadline in milliseconds.
        #[arg(long, default_value_t = 30_000, value_name = "MILLISECONDS")]
        timeout_ms: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum UpgradeChannel {
    Stable,
    Beta,
}

impl From<UpgradeChannel> for rw_core::UpdateChannel {
    fn from(value: UpgradeChannel) -> Self {
        match value {
            UpgradeChannel::Stable => Self::Stable,
            UpgradeChannel::Beta => Self::Beta,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum HistoryExportFormat {
    Markdown,
    Html,
    Json,
}

impl From<HistoryExportFormat> for history::TranscriptFormat {
    fn from(value: HistoryExportFormat) -> Self {
        match value {
            HistoryExportFormat::Markdown => Self::Markdown,
            HistoryExportFormat::Html => Self::Html,
            HistoryExportFormat::Json => Self::Json,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    /// List sessions from newest to oldest.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Alias for `list`, optimized for quickly finding a resume target.
    Recent {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Search {
        #[arg(value_name = "QUERY")]
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Authenticate one configured HTTP MCP server with Authorization Code + PKCE.
    Login { server: String },
}

#[derive(Debug, Subcommand)]
enum McpServerCommand {
    /// Serve one MCP connection over standard input/output.
    Stdio {
        /// Primary workspace exposed to the server; defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    /// Generate a deterministic plugin project.
    Scaffold {
        /// SDK language (currently `ts`).
        #[arg(long, default_value = "ts")]
        lang: String,
        /// Destination directory.
        #[arg(value_name = "PATH", default_value = "rottweiler-plugin")]
        path: PathBuf,
        /// Package and manifest name.
        #[arg(long)]
        name: Option<String>,
        /// Replace existing regular template files.
        #[arg(long)]
        force: bool,
    },
    /// Run a plugin under the development supervisor (experimental).
    Dev {
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Explicitly authorize direct local development execution.
        #[arg(long)]
        allow_dev_exec: bool,
    },
    /// Inspect configured plugins and their exact approval state.
    Status,
    /// Approve one exact executable/config/origin/manifest identity.
    Approve { name: String },
    /// Revoke one durable plugin approval.
    Revoke { name: String },
}

#[derive(Clone, Copy, Debug, Subcommand)]
enum TrustCommand {
    /// Show the exact executable inventory and its trust state.
    Status,
    /// Trust the exact currently displayed executable inventory.
    Grant,
    /// Revoke the current workspace decision.
    Revoke,
}

#[derive(Debug, Subcommand)]
enum PromptCommand {
    /// Print the exact provider-neutral request for the latest or selected turn.
    Dump {
        /// Historical agent turn to assemble; omitted selects the latest state.
        #[arg(long, value_name = "N")]
        turn: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate and print the effective configuration with provenance.
    Check {
        /// Apply a highest-precedence KEY=VALUE override.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        overrides: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// List concrete models from each configured provider's live catalog.
    List {
        /// Bypass the short process-local discovery cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Show one exact live `provider/model` record.
    Show {
        /// Concrete provider-qualified model id.
        id: String,
        /// Bypass the short process-local discovery cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Fetch, validate, and atomically install the latest model table.
    Refresh {
        /// Catalog URL. Remote sources must use HTTPS; loopback HTTP is allowed for tests.
        #[arg(long, default_value = DEFAULT_MODEL_CATALOG_URL)]
        source: String,
        /// Explicit destination; defaults to the user-scoped models.toml.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Run the configured provider's browser or device authorization flow.
    Login {
        /// User-configured provider name from `[providers.<name>]`.
        provider: String,
    },
    /// Store an API key from a hidden TTY prompt.
    SetKey {
        /// User-configured provider name from `[providers.<name>]`.
        provider: String,
    },
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    if maybe_run_sandbox_helper(std::env::args_os()).map_err(|error| miette!(error.to_string()))? {
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let mut cli = Cli::parse();
    upgrade::show_pending_release_notes();
    if let Some(host) = cli.remote.as_deref() {
        if cli.command.is_some() || cli.prompt.is_some() || cli.line {
            return Err(miette!("--remote is available only for the OpenTUI client"));
        }
        run_remote_tui(host, &cli).await?;
        return Ok(());
    }
    match cli.command.take() {
        Some(Command::InstallSync { paths }) => {
            sync_install_paths(&paths)?;
        }
        Some(Command::Serve {
            socket,
            token_file,
            session,
            workspace,
            wait_for_execution_lease,
        }) => {
            run_serve(
                socket,
                token_file,
                session,
                workspace,
                cli.permission_mode,
                cli.max_turns,
                cli.model,
                cli.detach,
                cli.add_dirs,
                cli.dangerously_trust,
                cli.in_memory_replay_script,
                cli.record_script_delay_ms,
                wait_for_execution_lease,
            )
            .await?;
        }
        Some(Command::Prompt {
            command: PromptCommand::Dump { turn },
        }) => {
            runtime::run(runtime::RunOptions {
                prompt: None,
                output_format: cli.output_format,
                permission_mode: cli.permission_mode,
                max_turns: cli.max_turns,
                resume: cli.resume.clone(),
                continue_latest: cli.resume.is_none(),
                replay_dir: None,
                record_replay_script: None,
                in_memory_replay_script: None,
                record_script_delay_ms: 0,
                perf_markers: false,
                replay_provider: "prompt-dump-offline".to_owned(),
                model: cli.model,
                additional_workspaces: cli.add_dirs,
                dangerously_trust: cli.dangerously_trust,
                action: runtime::RunAction::PromptDump { turn },
            })
            .await?;
        }
        Some(Command::Config {
            command: ConfigCommand::Check { overrides },
        }) => {
            let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
            let loader = if cli.dangerously_trust {
                loader.dangerously_trust_project()
            } else {
                loader
            };
            let effective = loader
                .with_cli_overrides(overrides)
                .load()
                .into_diagnostic()?;
            for warning in effective.warnings() {
                eprintln!("warning: {}", warning.message());
            }
            print!("{}", effective.render_with_provenance());
        }
        Some(Command::Models {
            command: ModelsCommand::Refresh { source, output },
        }) => {
            let report = refresh_model_catalog(&source, output)
                .await
                .into_diagnostic()?;
            for warning in &report.warnings {
                eprintln!("warning: {warning}");
            }
            println!(
                "refreshed {} models from {} into {}",
                report.model_count,
                report.source_url,
                report.path.display()
            );
        }
        Some(Command::Models {
            command: ModelsCommand::List { refresh },
        }) => {
            let catalog = runtime::discover_model_catalog(refresh).await?;
            if cli.output_format == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&catalog).into_diagnostic()?
                );
            } else {
                println!("Aliases:");
                for alias in &catalog.aliases {
                    println!("  {} -> {}", alias.alias.0, alias.candidates.join(", "));
                }
                println!("Models:");
                for model in &catalog.models {
                    let marker = if model.current { "*" } else { " " };
                    let state = if model.available {
                        "available"
                    } else {
                        "unavailable"
                    };
                    println!("{marker} {}  {}  {state}", model.id, model.display_name);
                    if let Some(status) = &model.status {
                        println!("    {status}");
                    }
                }
                println!("Providers:");
                for provider in &catalog.providers {
                    println!(
                        "  {}  {:?}  models={}  {}",
                        provider.name,
                        provider.auth_kind,
                        provider.model_count,
                        provider.status.as_deref().unwrap_or("ready")
                    );
                }
            }
        }
        Some(Command::Models {
            command: ModelsCommand::Show { id, refresh },
        }) => {
            let catalog = runtime::discover_model_catalog(refresh).await?;
            let model = catalog
                .models
                .iter()
                .find(|model| model.id == id)
                .ok_or_else(|| miette!("model {id:?} is not present in the live catalog"))?;
            if cli.output_format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(model).into_diagnostic()?);
            } else {
                println!("id: {}", model.id);
                println!("name: {}", model.display_name);
                println!("provider: {}", model.provider);
                println!("available: {}", model.available);
                println!(
                    "aliases: {}",
                    model
                        .aliases
                        .iter()
                        .map(|alias| alias.0.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                println!("tool_calling: {}", model.capabilities.tool_calling);
                println!("vision: {}", model.capabilities.vision);
                println!("thinking: {}", model.capabilities.thinking);
                println!(
                    "max_context_tokens: {}",
                    model
                        .capabilities
                        .max_context_tokens
                        .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
                );
                println!(
                    "max_output_tokens: {}",
                    model
                        .capabilities
                        .max_output_tokens
                        .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
                );
                if let Some(status) = &model.status {
                    println!("status: {status}");
                }
            }
        }
        Some(Command::Auth {
            command: AuthCommand::Login { provider },
        }) => auth_login(&provider).await?,
        Some(Command::Auth {
            command: AuthCommand::SetKey { provider },
        }) => auth_set_key(&provider)?,
        Some(Command::Trust { command }) => run_trust_command(command)?,
        Some(Command::Plugin {
            command:
                PluginCommand::Scaffold {
                    lang,
                    path,
                    name,
                    force,
                },
        }) => {
            if lang != "ts" {
                return Err(miette!(
                    "unsupported plugin scaffold language {lang:?}; expected ts"
                ));
            }
            for written in plugin_cli::scaffold_typescript(&path, name.as_deref(), force)? {
                println!("{}", written.display());
            }
        }
        Some(Command::Plugin {
            command: PluginCommand::Status,
        }) => run_plugin_approval(None, false)?,
        Some(Command::Plugin {
            command: PluginCommand::Approve { name },
        }) => run_plugin_approval(Some(&name), false)?,
        Some(Command::Plugin {
            command: PluginCommand::Revoke { name },
        }) => run_plugin_approval(Some(&name), true)?,
        Some(Command::Plugin {
            command:
                PluginCommand::Dev {
                    path,
                    allow_dev_exec,
                },
        }) => {
            if !allow_dev_exec {
                return Err(miette!(
                    "plugin dev executes local code; pass --allow-dev-exec to grant explicit development authority"
                ));
            }
            plugin_dev::run(&path).await?;
        }
        Some(Command::McpServer {
            command: McpServerCommand::Stdio { workspace },
        }) => {
            let workspace = workspace.unwrap_or(std::env::current_dir().into_diagnostic()?);
            let workspace_roots = canonical_workspace_roots(&workspace, &cli.add_dirs)?;
            let provider_mode = if let Some(script) = cli.in_memory_replay_script.as_deref() {
                runtime::HostedProviderMode::DeterministicReplay {
                    provider_name: "mcp-server-replay".to_owned(),
                    scripts: serde_json::from_slice(&fs::read(script).into_diagnostic()?)
                        .into_diagnostic()?,
                    event_delay_ms: cli.record_script_delay_ms,
                }
            } else {
                runtime::HostedProviderMode::Live
            };
            let options = host_runtime::CliHostOptions::from_environment(
                workspace_roots,
                cli.dangerously_trust,
                cli.permission_mode,
                cli.max_turns,
                provider_mode,
                false,
            )
            .map_err(|_| miette!("MCP server configuration could not initialize"))?;
            mcp_server::run_stdio(mcp_server::StdioServerOptions {
                workspace_roots: options.allowed_workspaces,
                storage_root: options.storage_root,
                credentials_path: options.credentials_path,
                config: options.config,
                permission_mode: options.permission_mode,
                max_turns: options.max_turns,
                provider_mode: options.provider_mode,
                dangerously_trust: options.dangerously_trust,
            })
            .await?;
        }
        Some(Command::Mcp {
            command: McpCommand::Login { server },
        }) => mcp_cli::login(&server, cli.dangerously_trust).await?,
        Some(Command::Replay { session, jsonl }) => {
            let storage_root = configuration_root_path()?;
            let events = history::load_events(&storage_root, &session)?;
            if jsonl {
                io::stdout()
                    .write_all(&history::replay_jsonl(&events)?)
                    .into_diagnostic()?;
            } else {
                run_history_replay(&storage_root, &session, events).await?;
            }
        }
        Some(Command::Export {
            session,
            format,
            output,
            force,
        }) => {
            let storage_root = configuration_root_path()?;
            let events = history::load_events(&storage_root, &session)?;
            let redactor = rw_core::runtime_support::FixtureRedactor::default();
            runtime::register_credential_environment(&redactor);
            let exported = history::export_transcript(&session, &events, format.into(), &redactor)?;
            if let Some(path) = output {
                write_history_export(&storage_root, &path, &exported, force)?;
            } else {
                io::stdout().write_all(&exported).into_diagnostic()?;
            }
        }
        Some(Command::Sessions {
            command: SessionsCommand::Search { query, limit },
        }) => {
            let sessions = history::search_sessions(&configuration_root_path()?, &query, limit)?;
            render_session_search(&sessions, cli.output_format)?;
        }
        Some(Command::Sessions {
            command: SessionsCommand::List { limit } | SessionsCommand::Recent { limit },
        }) => {
            let sessions = history::list_sessions(&configuration_root_path()?, limit)?;
            render_session_search(&sessions, cli.output_format)?;
        }
        Some(Command::Stats {
            session,
            from,
            through,
            json,
        }) => {
            let report = stats::collect(
                &configuration_root_path()?,
                &stats::StatsQuery {
                    session,
                    from_day: from,
                    through_day: through,
                },
            )?;
            if json
                || matches!(
                    cli.output_format,
                    OutputFormat::Json | OutputFormat::StreamJson
                )
            {
                println!("{}", serde_json::to_string(&report).into_diagnostic()?);
            } else {
                print!("{}", stats::render_text(&report));
            }
        }
        Some(Command::Import {
            source,
            source_root,
            target,
            dry_run,
            json,
        }) => {
            let target_root = target.unwrap_or(std::env::current_dir().into_diagnostic()?);
            let report = import::run(&import::ImportOptions {
                source,
                source_root,
                target_root,
                dry_run,
            })?;
            if json
                || matches!(
                    cli.output_format,
                    OutputFormat::Json | OutputFormat::StreamJson
                )
            {
                println!("{}", serde_json::to_string(&report).into_diagnostic()?);
            } else {
                for item in report.items {
                    println!("{:?}\t{}\t{}", item.status, item.target, item.detail);
                }
            }
        }
        Some(Command::Doctor {
            network,
            timeout_ms,
            json,
        }) => {
            let report = doctor::collect(doctor::DoctorOptions {
                network,
                timeout_ms,
            })
            .await;
            if json
                || matches!(
                    cli.output_format,
                    OutputFormat::Json | OutputFormat::StreamJson
                )
            {
                println!("{}", serde_json::to_string(&report).into_diagnostic()?);
            } else {
                print!("{}", doctor::render_text(&report));
            }
            if report.has_failures() {
                return Err(miette!("doctor found one or more blocking issues"));
            }
        }
        Some(Command::Upgrade {
            channel,
            allow_downgrade,
            rollback,
            timeout_ms,
        }) => {
            upgrade::run(upgrade::UpgradeOptions {
                channel: channel.map(Into::into),
                allow_downgrade,
                rollback,
                timeout_ms,
            })
            .await?;
        }
        None => {
            let headless_or_line = cli.prompt.is_some()
                || cli.line
                || cli.replay_dir.is_some()
                || cli.record_replay_script.is_some()
                || cli.perf_markers;
            if headless_or_line {
                runtime::run(runtime::RunOptions {
                    prompt: cli.prompt,
                    output_format: cli.output_format,
                    permission_mode: cli.permission_mode,
                    max_turns: cli.max_turns,
                    resume: cli.resume,
                    continue_latest: cli.continue_latest,
                    replay_dir: cli.replay_dir,
                    record_replay_script: cli.record_replay_script,
                    in_memory_replay_script: cli.in_memory_replay_script,
                    record_script_delay_ms: cli.record_script_delay_ms,
                    perf_markers: cli.perf_markers,
                    replay_provider: cli.replay_provider,
                    model: cli.model,
                    additional_workspaces: cli.add_dirs,
                    dangerously_trust: cli.dangerously_trust,
                    action: runtime::RunAction::Agent,
                })
                .await?;
            } else {
                run_local_tui(&cli).await?;
            }
        }
    }

    Ok(())
}

fn write_history_export(
    storage_root: &Path,
    output: &Path,
    bytes: &[u8],
    force: bool,
) -> Result<()> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).into_diagnostic()?;
    let filename = output
        .file_name()
        .ok_or_else(|| miette!("export output must name a file"))?;
    if let Ok(canonical_storage) = fs::canonicalize(storage_root)
        && parent.starts_with(canonical_storage)
    {
        return Err(miette!("export output cannot modify Rottweiler storage"));
    }

    #[cfg(unix)]
    return write_history_export_unix(&parent, filename, bytes, force, || Ok(()));

    #[cfg(not(unix))]
    write_history_export_portable(storage_root, &parent, filename, bytes, force)
}

#[cfg(unix)]
fn write_history_export_unix(
    parent: &Path,
    filename: &std::ffi::OsStr,
    bytes: &[u8],
    force: bool,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};

    let expected = fs::metadata(parent).into_diagnostic()?;
    let directory = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let opened = rustix::fs::fstat(&directory)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    {
        use std::os::unix::fs::MetadataExt as _;
        if Some(expected.dev()) != rustix_device_id(opened.st_dev)
            || expected.ino() != opened.st_ino
        {
            return Err(miette!(
                "export output directory changed while it was opened"
            ));
        }
    }
    match rustix::fs::statat(&directory, filename, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            if !force {
                return Err(miette!(
                    "export output already exists; pass --force to replace it"
                ));
            }
            if !FileType::from_raw_mode(stat.st_mode).is_file() {
                return Err(miette!("export output is not a regular file"));
            }
            if stat.st_nlink != 1 {
                return Err(miette!("export output has multiple hard links"));
            }
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(std::io::Error::from(error)).into_diagnostic(),
    }
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).into_diagnostic()?;
    let temporary = format!(
        ".rottweiler-export-{}-{}",
        std::process::id(),
        u64::from_ne_bytes(random)
    );
    let descriptor = rustix::fs::openat(
        &directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let result = (|| -> Result<()> {
        let mut file = fs::File::from(descriptor);
        file.write_all(bytes).into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        before_commit()?;
        if force {
            rustix::fs::renameat(&directory, temporary.as_str(), &directory, filename)
        } else {
            rustix::fs::renameat_with(
                &directory,
                temporary.as_str(),
                &directory,
                filename,
                RenameFlags::NOREPLACE,
            )
        }
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
        rustix::fs::fsync(&directory)
            .map_err(std::io::Error::from)
            .into_diagnostic()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), AtFlags::empty());
    }
    result
}

#[cfg(not(unix))]
fn write_history_export_portable(
    storage_root: &Path,
    parent: &Path,
    filename: &std::ffi::OsStr,
    bytes: &[u8],
    force: bool,
) -> Result<()> {
    let destination = parent.join(filename);
    if destination.exists() {
        let message = if force {
            "safe --force replacement is unavailable on this platform"
        } else {
            "export output already exists; pass --force to replace it"
        };
        return Err(miette!(message));
    }
    let parent = fs::canonicalize(parent).into_diagnostic()?;
    if let Ok(canonical_storage) = fs::canonicalize(storage_root)
        && parent.starts_with(canonical_storage)
    {
        return Err(miette!("export output cannot modify Rottweiler storage"));
    }
    let destination = parent.join(filename);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()
}

fn render_session_search(
    sessions: &[rw_store::session::SessionSummary],
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Text => {
            for session in sessions {
                let title = session
                    .title
                    .chars()
                    .map(|character| {
                        if character.is_control() {
                            ' '
                        } else {
                            character
                        }
                    })
                    .collect::<String>();
                println!(
                    "{}\t{}\t{}\t{}",
                    session.id, session.updated_unix_ms, session.cost_micros, title
                );
            }
        }
        OutputFormat::Json => {
            let values = sessions
                .iter()
                .map(|session| {
                    serde_json::json!({
                        "id":session.id,"title":session.title,
                        "updated_unix_ms":session.updated_unix_ms,"cost_micros":session.cost_micros,
                    })
                })
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string(&values).into_diagnostic()?);
        }
        OutputFormat::StreamJson => {
            for session in sessions {
                println!(
                    "{}",
                    serde_json::json!({
                        "id":session.id,"title":session.title,
                        "updated_unix_ms":session.updated_unix_ms,"cost_micros":session.cost_micros,
                    })
                );
            }
        }
    }
    Ok(())
}

async fn run_local_tui(cli: &Cli) -> Result<()> {
    let workspace =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let storage_root = configuration_root()?;
    let workspace_roots = canonical_workspace_roots(&workspace, &cli.add_dirs)?;
    prompt_for_folder_trust(&storage_root, &workspace_roots, cli.dangerously_trust)?;
    let project_assessment =
        rw_store::trust::FolderTrustStore::new(storage_root.join("trust.json"))
            .assess(&workspace)
            .into_diagnostic()?;
    let project_inventory = (cli.dangerously_trust
        || project_assessment.project_execution_enabled())
    .then(|| project_assessment.inventory());
    let (user_home, user_rottweiler) =
        runtime::extension_user_roots(&storage_root.join("credentials.toml"));
    let tui_keybindings = tui_config::load_keybindings(
        Some(&workspace),
        project_inventory,
        &user_home,
        &user_rottweiler,
    )
    .map_err(|error| miette!(error.to_string()))?;
    let tui_theme = rw_store::config::ConfigLoader::from_environment()
        .into_diagnostic()?
        .with_project_trust(cli.dangerously_trust || project_assessment.project_execution_enabled())
        .load()
        .into_diagnostic()?
        .config
        .ui
        .theme;
    let session_id = runtime::select_interactive_session(
        &storage_root,
        &workspace,
        cli.resume.as_deref(),
        cli.continue_latest,
    )?;
    if (cli.resume.is_some() || cli.continue_latest)
        && !session_metadata_path(&storage_root, &session_id).is_file()
    {
        return Err(miette!("session {session_id:?} does not exist"));
    }
    let paths = allocate_runtime_paths(&storage_root.join("run"))?;
    let mut runtime_directory = RuntimeDirectoryGuard::capture(&paths.directory)?;
    let supervisor = supervisor::Supervisor::new(
        supervisor::SupervisorConfig {
            rw_executable: std::env::current_exe().into_diagnostic()?,
            tui_executable: locate_tui_executable()?,
            socket: paths.socket,
            token_file: paths.token,
            last_seen_file: paths.directory.join("last-seen"),
            fork_operation_directory: storage_root.join("control/pending-forks"),
            session_id,
            tui_keybindings,
            tui_theme,
            permission_mode: cli.permission_mode,
            max_turns: cli.max_turns,
            model: cli.model.clone(),
            additional_workspaces: workspace_roots.into_iter().skip(1).collect(),
            dangerously_trust: cli.dangerously_trust,
            in_memory_replay_script: cli.in_memory_replay_script.clone(),
            record_script_delay_ms: cli.record_script_delay_ms,
            shell_target: Some(shell_broker::ShellTarget::Local),
            detach: cli.detach,
            restart_policy: supervisor::RestartPolicy::default(),
        },
        supervisor::TokioProcessBackend,
        supervisor::ResumeHandoff::default(),
    )
    .map_err(|error| miette!(error.to_string()))?;
    let result = supervisor
        .run()
        .await
        .map_err(|error| miette!(error.to_string()));
    if cli.detach && result.is_ok() {
        runtime_directory.preserve();
    }
    result
}

#[derive(Clone, Default)]
struct DeferredHostedEngine {
    inner: Arc<tokio::sync::RwLock<Option<server::HostedEngine>>>,
    ready: Arc<AtomicBool>,
}

impl DeferredHostedEngine {
    async fn install(&self, engine: server::HostedEngine) {
        *self.inner.write().await = Some(engine);
        self.ready.store(true, Ordering::Release);
    }

    async fn loaded(&self) -> std::result::Result<server::HostedEngine, String> {
        self.inner
            .read()
            .await
            .clone()
            .ok_or_else(|| "engine session runtime is still starting".to_owned())
    }
}

#[derive(Clone)]
struct HistoricalReplayEngine {
    session_id: SessionId,
    events: Arc<Vec<HistoricalReplayItem>>,
    through_sequence: Option<SequenceId>,
}

#[derive(Clone, Debug)]
enum HistoricalReplayItem {
    Durable(rw_store::session::EventEnvelope<EngineEvent>),
    Progress {
        parent_cursor: SequenceId,
        event: EngineEvent,
    },
}

const MAX_REPLAY_CHILD_DEPTH: usize = 8;
const MAX_REPLAY_CHILD_SESSIONS: usize = 1_024;
const MAX_REPLAY_PROGRESS_BYTES: usize = 256 * 1024;

struct HistoricalReplayBudget {
    bytes: u64,
    events: usize,
    sessions: usize,
}

impl HistoricalReplayBudget {
    fn consume(&mut self, value: &serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(value).into_diagnostic()?;
        if bytes.len() > MAX_REPLAY_PROGRESS_BYTES {
            return Err(miette!("historical child progress exceeds its size limit"));
        }
        let length = u64::try_from(bytes.len()).into_diagnostic()?;
        self.bytes = self
            .bytes
            .checked_sub(length)
            .ok_or_else(|| miette!("historical child replay exceeds its byte limit"))?;
        self.events = self
            .events
            .checked_sub(1)
            .ok_or_else(|| miette!("historical child replay exceeds its event limit"))?;
        Ok(())
    }
}

#[async_trait]
impl server::ServerEngine for HistoricalReplayEngine {
    async fn dispatch(
        &self,
        _bound_client: ClientId,
        command: ClientCommand,
    ) -> std::result::Result<CommandOutcome, String> {
        match command {
            ClientCommand::AttachSession {
                session_id,
                role: rw_core::ClientRole::Observer,
                ..
            } if session_id == self.session_id => Ok(CommandOutcome::Accepted),
            _ => Ok(CommandOutcome::Rejected {
                error: rw_core::EngineError {
                    category: rw_core::EngineErrorCategory::Protocol,
                    code: "historical_replay_read_only".to_owned(),
                    message: "historical replay accepts only observer attachment".to_owned(),
                    retryable: false,
                    details: None,
                },
            }),
        }
    }

    async fn subscribe(
        &self,
        bound_client: ClientId,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<EngineEvent, String>>,
        String,
    > {
        if session_id.as_ref() != Some(&self.session_id) {
            return Err("historical replay session mismatch".to_owned());
        }
        let events = Arc::clone(&self.events);
        let replay_session = self.session_id.clone();
        let through_sequence = self.through_sequence;
        let (sender, receiver) = tokio::sync::mpsc::channel(256);
        tokio::spawn(async move {
            for item in events.iter() {
                let event = match item {
                    HistoricalReplayItem::Durable(envelope)
                        if last_seen.is_none_or(|sequence| envelope.sequence.0 > sequence.0) =>
                    {
                        &envelope.event
                    }
                    HistoricalReplayItem::Progress {
                        parent_cursor,
                        event,
                    } if last_seen.is_none_or(|sequence| parent_cursor.0 > sequence.0) => event,
                    _ => continue,
                };
                if sender.send(Ok(event.clone())).await.is_err() {
                    return;
                }
            }
            let _ = sender
                .send(Ok(EngineEvent::SessionReplayCompleted {
                    meta: rw_core::CommandAckMeta {
                        protocol_version: rw_core::PROTOCOL_VERSION,
                        client_id: bound_client,
                        request_id: rw_core::RequestId("historical-replay".to_owned()),
                        emitted_at: "1970-01-01T00:00:00Z".to_owned(),
                    },
                    session_id: replay_session,
                    through_sequence,
                }))
                .await;
        });
        Ok(receiver)
    }

    async fn complete_shell(
        &self,
        _session_id: SessionId,
        _shell_id: rw_core::ShellId,
        _status: i32,
        _captured_output: Option<String>,
    ) -> std::result::Result<(), String> {
        Err("historical replay is read-only".to_owned())
    }
}

async fn run_history_replay(
    storage_root: &Path,
    session: &str,
    events: Vec<rw_store::session::EventEnvelope<EngineEvent>>,
) -> Result<()> {
    let tui = locate_tui_executable()?;
    run_history_replay_with_tui(storage_root, session, events, &tui).await
}

async fn run_history_replay_with_tui(
    storage_root: &Path,
    session: &str,
    events: Vec<rw_store::session::EventEnvelope<EngineEvent>>,
    tui: &Path,
) -> Result<()> {
    let (user_home, user_rottweiler) =
        runtime::extension_user_roots(&storage_root.join("credentials.toml"));
    let keybindings = tui_config::load_keybindings(None, None, &user_home, &user_rottweiler)
        .map_err(|error| miette!(error.to_string()))?;
    let through_sequence = events.last().map(|envelope| envelope.sequence);
    let events = historical_replay_items(storage_root, session, events)?;
    let paths = allocate_runtime_paths(&storage_root.join("run"))?;
    let _runtime_directory = RuntimeDirectoryGuard::capture(&paths.directory)?;
    let (runtime, listener) = server::ServerRuntime::create_for_session(paths, Some(session))?;
    let state = server::ServerState::new(
        Arc::new(HistoricalReplayEngine {
            session_id: SessionId(session.to_owned()),
            events: Arc::new(events),
            through_sequence,
        }),
        &runtime,
    );
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let server_task = tokio::spawn(server::serve(listener, state, shutdown_rx));
    let mut command = tokio::process::Command::new(tui);
    command
        .env_remove("ROTTWEILER_TUI_KEYBINDINGS")
        .env("ROTTWEILER_ENGINE_SOCKET", &runtime.paths.socket)
        .env("ROTTWEILER_ENGINE_TOKEN_FILE", &runtime.paths.token)
        .env("ROTTWEILER_SESSION_ID", session)
        .env("ROTTWEILER_REPLAY_MODE", "1")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    if let Some(keybindings) = keybindings {
        command.env("ROTTWEILER_TUI_KEYBINDINGS", keybindings);
    }
    let status = command.status().await;
    let _ = shutdown.send(true);
    server_task.await.into_diagnostic()??;
    drop(runtime);
    let status = status.into_diagnostic()?;
    if !status.success() {
        return Err(miette!("historical replay TUI exited with status {status}"));
    }
    Ok(())
}

fn historical_replay_items(
    storage_root: &Path,
    session: &str,
    events: Vec<rw_store::session::EventEnvelope<EngineEvent>>,
) -> Result<Vec<HistoricalReplayItem>> {
    let mut output = Vec::new();
    let mut budget = HistoricalReplayBudget {
        bytes: history::MAX_HISTORY_BYTES,
        events: history::MAX_HISTORY_EVENTS,
        sessions: MAX_REPLAY_CHILD_SESSIONS,
    };
    let root_session = SessionId(session.to_owned());
    let mut ancestors = HashSet::from([root_session.clone()]);
    for envelope in events {
        let cursor = envelope.sequence;
        let spawned = match &envelope.event {
            EngineEvent::SubagentSpawned {
                subagent_id,
                child_session_id,
                ..
            } => Some((subagent_id.clone(), child_session_id.clone())),
            _ => None,
        };
        output.push(HistoricalReplayItem::Durable(envelope));
        if let Some((subagent_id, child_session_id)) = spawned {
            let child = historical_child_stream(
                storage_root,
                &child_session_id,
                &mut budget,
                &mut ancestors,
                1,
            )?;
            for (child_sequence, event) in child {
                let event = EngineEvent::SubagentProgress {
                    parent_session_id: root_session.clone(),
                    subagent_id: subagent_id.clone(),
                    child_session_id: child_session_id.clone(),
                    child_sequence,
                    event,
                };
                budget.consume(&serde_json::to_value(&event).into_diagnostic()?)?;
                output.push(HistoricalReplayItem::Progress {
                    parent_cursor: cursor,
                    event,
                });
            }
        }
    }
    Ok(output)
}

fn historical_child_stream(
    storage_root: &Path,
    session: &SessionId,
    budget: &mut HistoricalReplayBudget,
    ancestors: &mut HashSet<SessionId>,
    depth: usize,
) -> Result<Vec<(Option<SequenceId>, serde_json::Value)>> {
    if depth > MAX_REPLAY_CHILD_DEPTH {
        return Err(miette!("historical child replay exceeds its nesting limit"));
    }
    budget.sessions = budget
        .sessions
        .checked_sub(1)
        .ok_or_else(|| miette!("historical child replay exceeds its session limit"))?;
    if !ancestors.insert(session.clone()) {
        return Err(miette!("historical child replay contains a session cycle"));
    }
    let result = (|| {
        let events = rw_store::session::SessionEventLog::load_existing_bounded::<EngineEvent>(
            storage_root,
            &session.0,
            budget.bytes,
            budget.events,
        )
        .map_err(|error| miette!("historical child session could not be read: {error}"))?;
        let mut output = Vec::new();
        for envelope in events {
            let meta = envelope
                .event
                .meta()
                .ok_or_else(|| miette!("historical child log contains a non-durable event"))?;
            if meta.session_id != *session || meta.sequence_id != envelope.sequence {
                return Err(miette!(
                    "historical child event identity does not match its durable envelope"
                ));
            }
            let spawned = match &envelope.event {
                EngineEvent::SubagentSpawned {
                    subagent_id,
                    child_session_id,
                    ..
                } => Some((subagent_id.clone(), child_session_id.clone())),
                _ => None,
            };
            let value = serde_json::to_value(&envelope.event).into_diagnostic()?;
            budget.consume(&value)?;
            output.push((Some(envelope.sequence), value));
            if let Some((subagent_id, child_session_id)) = spawned {
                for (child_sequence, event) in historical_child_stream(
                    storage_root,
                    &child_session_id,
                    budget,
                    ancestors,
                    depth + 1,
                )? {
                    let progress = EngineEvent::SubagentProgress {
                        parent_session_id: session.clone(),
                        subagent_id: subagent_id.clone(),
                        child_session_id: child_session_id.clone(),
                        child_sequence,
                        event,
                    };
                    let value = serde_json::to_value(progress).into_diagnostic()?;
                    budget.consume(&value)?;
                    output.push((None, value));
                }
            }
        }
        Ok(output)
    })();
    ancestors.remove(session);
    result
}

#[cfg(test)]
mod historical_replay_tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use rw_core::runtime_support::SubagentId;
    use rw_core::{ClientRole, CommandMeta, EventMeta, RequestId};
    use rw_store::session::SessionEventLog;

    fn meta() -> CommandMeta {
        CommandMeta {
            protocol_version: rw_core::PROTOCOL_VERSION,
            client_id: ClientId("client".to_owned()),
            request_id: RequestId("request".to_owned()),
        }
    }

    fn event_meta(session: &str, sequence: u64) -> EventMeta {
        EventMeta {
            protocol_version: rw_core::PROTOCOL_VERSION,
            session_id: SessionId(session.to_owned()),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        }
    }

    fn engine() -> HistoricalReplayEngine {
        let session_id = SessionId("history".to_owned());
        HistoricalReplayEngine {
            session_id: session_id.clone(),
            events: Arc::new(vec![HistoricalReplayItem::Durable(
                rw_store::session::EventEnvelope {
                    schema_version: 1,
                    sequence: SequenceId(0),
                    event: EngineEvent::UiNotification {
                        meta: EventMeta {
                            protocol_version: rw_core::PROTOCOL_VERSION,
                            session_id,
                            sequence_id: SequenceId(0),
                            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                            caused_by: None,
                        },
                        plugin_id: "fixture".to_owned(),
                        title: "title".to_owned(),
                        message: "message".to_owned(),
                    },
                },
            )]),
            through_sequence: Some(SequenceId(0)),
        }
    }

    #[tokio::test]
    async fn historical_replay_is_ordered_and_strictly_read_only() {
        let engine = engine();
        let observer = ClientCommand::AttachSession {
            meta: meta(),
            session_id: SessionId("history".to_owned()),
            last_seen_sequence: None,
            role: ClientRole::Observer,
        };
        assert_eq!(
            server::ServerEngine::dispatch(&engine, ClientId("bound".to_owned()), observer).await,
            Ok(CommandOutcome::Accepted)
        );
        let driver = ClientCommand::AttachSession {
            meta: meta(),
            session_id: SessionId("history".to_owned()),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        };
        assert!(matches!(
            server::ServerEngine::dispatch(&engine, ClientId("bound".to_owned()), driver).await,
            Ok(CommandOutcome::Rejected { .. })
        ));
        assert!(matches!(
            server::ServerEngine::dispatch(
                &engine,
                ClientId("bound".to_owned()),
                ClientCommand::Interrupt {
                    meta: meta(),
                    session_id: SessionId("history".to_owned()),
                },
            )
            .await,
            Ok(CommandOutcome::Rejected { .. })
        ));
        assert!(
            server::ServerEngine::subscribe(
                &engine,
                ClientId("bound".to_owned()),
                Some(SessionId("wrong".to_owned())),
                None,
            )
            .await
            .is_err()
        );
        let mut replay = server::ServerEngine::subscribe(
            &engine,
            ClientId("bound".to_owned()),
            Some(SessionId("history".to_owned())),
            None,
        )
        .await
        .expect("subscribe");
        assert!(matches!(
            replay.recv().await,
            Some(Ok(EngineEvent::UiNotification { .. }))
        ));
        assert!(matches!(
            replay.recv().await,
            Some(Ok(EngineEvent::SessionReplayCompleted {
                through_sequence: Some(SequenceId(0)),
                ..
            }))
        ));
        assert!(replay.recv().await.is_none());
    }

    #[test]
    fn historical_replay_rederives_bounded_nested_child_progress() {
        let storage = tempfile::tempdir().expect("storage");
        let mut grandchild =
            SessionEventLog::open(storage.path(), "grandchild").expect("grandchild log");
        grandchild
            .append(EngineEvent::UiNotification {
                meta: event_meta("grandchild", 0),
                plugin_id: "fixture".to_owned(),
                title: "grandchild".to_owned(),
                message: "working".to_owned(),
            })
            .expect("grandchild event");
        let mut child = SessionEventLog::open(storage.path(), "child").expect("child log");
        child
            .append_batch([
                EngineEvent::UiNotification {
                    meta: event_meta("child", 0),
                    plugin_id: "fixture".to_owned(),
                    title: "child".to_owned(),
                    message: "working".to_owned(),
                },
                EngineEvent::SubagentSpawned {
                    meta: event_meta("child", 1),
                    subagent_id: SubagentId("nested".to_owned()),
                    child_session_id: SessionId("grandchild".to_owned()),
                    task: "nested task".to_owned(),
                },
            ])
            .expect("child events");
        let root = rw_store::session::EventEnvelope {
            schema_version: 1,
            sequence: SequenceId(0),
            event: EngineEvent::SubagentSpawned {
                meta: event_meta("root", 0),
                subagent_id: SubagentId("direct".to_owned()),
                child_session_id: SessionId("child".to_owned()),
                task: "direct task".to_owned(),
            },
        };

        let replay = historical_replay_items(storage.path(), "root", vec![root])
            .expect("derived historical replay");
        assert_eq!(replay.len(), 4);
        let HistoricalReplayItem::Progress { event, .. } = &replay[3] else {
            panic!("nested progress wrapper");
        };
        let EngineEvent::SubagentProgress { event, .. } = event else {
            panic!("direct progress wrapper");
        };
        assert_eq!(event["type"], "subagent_progress");
        assert_eq!(event["event"]["type"], "ui_notification");
        assert_eq!(event["event"]["title"], "grandchild");
    }

    #[test]
    fn historical_replay_charges_root_progress_wrapper_amplification() {
        let storage = tempfile::tempdir().expect("storage");
        let mut child = SessionEventLog::open(storage.path(), "child").expect("child log");
        child
            .append(EngineEvent::UiNotification {
                meta: event_meta("child", 0),
                plugin_id: "fixture".to_owned(),
                title: "child".to_owned(),
                message: "small event".to_owned(),
            })
            .expect("child event");
        let root = rw_store::session::EventEnvelope {
            schema_version: 1,
            sequence: SequenceId(0),
            event: EngineEvent::SubagentSpawned {
                meta: event_meta("root", 0),
                subagent_id: SubagentId("x".repeat(MAX_REPLAY_PROGRESS_BYTES)),
                child_session_id: SessionId("child".to_owned()),
                task: "direct task".to_owned(),
            },
        };

        let error = historical_replay_items(storage.path(), "root", vec![root])
            .expect_err("amplified root wrapper must be bounded");
        assert!(error.to_string().contains("progress exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn historical_replay_rejects_symlinked_child_logs() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().expect("storage");
        let outside = tempfile::tempdir().expect("outside");
        let mut foreign =
            SessionEventLog::open(outside.path(), "foreign").expect("foreign child log");
        foreign
            .append(EngineEvent::UiNotification {
                meta: event_meta("foreign", 0),
                plugin_id: "fixture".to_owned(),
                title: "foreign".to_owned(),
                message: "must not load".to_owned(),
            })
            .expect("foreign event");
        fs::create_dir_all(storage.path().join("sessions")).expect("session root");
        symlink(
            outside.path().join("sessions/foreign"),
            storage.path().join("sessions/child"),
        )
        .expect("child symlink");
        let root = rw_store::session::EventEnvelope {
            schema_version: 1,
            sequence: SequenceId(0),
            event: EngineEvent::SubagentSpawned {
                meta: event_meta("root", 0),
                subagent_id: SubagentId("direct".to_owned()),
                child_session_id: SessionId("child".to_owned()),
                task: "direct task".to_owned(),
            },
        };

        let error = historical_replay_items(storage.path(), "root", vec![root])
            .expect_err("symlinked child must fail closed");
        assert!(error.to_string().contains("historical child session"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn historical_replay_process_gets_read_only_runtime_and_leaves_no_build_junk() {
        use std::os::unix::fs::PermissionsExt as _;

        let storage = tempfile::tempdir().expect("storage");
        let fixture = storage.path().join("fixture-tui");
        fs::write(storage.path().join("keybindings.toml"), "preset = 'vim'")
            .expect("user keybindings");
        fs::write(
            &fixture,
            b"#!/bin/sh\n\
              test \"$ROTTWEILER_REPLAY_MODE\" = \"1\" || exit 11\n\
              test \"$ROTTWEILER_SESSION_ID\" = \"history\" || exit 12\n\
              test -S \"$ROTTWEILER_ENGINE_SOCKET\" || exit 13\n\
              test -f \"$ROTTWEILER_ENGINE_TOKEN_FILE\" || exit 14\n\
              test \"$ROTTWEILER_TUI_KEYBINDINGS\" = \"preset = 'vim'\" || exit 15\n",
        )
        .expect("fixture script");
        fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700))
            .expect("fixture permissions");

        run_history_replay_with_tui(
            storage.path(),
            "history",
            engine()
                .events
                .iter()
                .filter_map(|item| match item {
                    HistoricalReplayItem::Durable(envelope) => Some(envelope.clone()),
                    HistoricalReplayItem::Progress { .. } => None,
                })
                .collect(),
            &fixture,
        )
        .await
        .expect("process replay");

        let mut runtime_entries = fs::read_dir(storage.path().join("run")).expect("runtime root");
        assert!(runtime_entries.next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn export_refuses_symlink_or_storage_targets_without_mutating_events() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().expect("storage");
        let session = storage.path().join("sessions/history");
        fs::create_dir_all(&session).expect("session directory");
        let events = session.join("events.jsonl");
        fs::write(&events, b"canary").expect("events");
        let output = tempfile::tempdir().expect("output");
        let planted = output.path().join("transcript.md");
        symlink(&events, &planted).expect("planted symlink");
        assert!(write_history_export(storage.path(), &planted, b"replacement", true).is_err());
        assert_eq!(fs::read(&events).expect("events unchanged"), b"canary");
        assert!(
            write_history_export(storage.path(), &session.join("export.md"), b"x", false).is_err()
        );
        assert_eq!(
            fs::read(&events).expect("events still unchanged"),
            b"canary"
        );
    }

    #[cfg(unix)]
    #[test]
    fn export_parent_swap_stays_bound_to_the_opened_directory_descriptor() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().expect("storage");
        let session = storage.path().join("sessions/history");
        fs::create_dir_all(&session).expect("session directory");
        let events = session.join("events.jsonl");
        fs::write(&events, b"event-canary").expect("events");

        let output = tempfile::tempdir().expect("output");
        let parent = output.path().join("safe");
        let moved = output.path().join("moved");
        fs::create_dir(&parent).expect("safe parent");
        let canonical_parent = fs::canonicalize(&parent).expect("canonical parent");
        let parent_for_swap = parent.clone();
        let moved_for_swap = moved.clone();
        let session_for_swap = session.clone();
        write_history_export_unix(
            &canonical_parent,
            std::ffi::OsStr::new("transcript.md"),
            b"safe export",
            false,
            move || {
                fs::rename(&parent_for_swap, &moved_for_swap).into_diagnostic()?;
                symlink(&session_for_swap, &parent_for_swap).into_diagnostic()?;
                Ok(())
            },
        )
        .expect("descriptor-bound export");

        assert_eq!(
            fs::read(moved.join("transcript.md")).expect("export"),
            b"safe export"
        );
        assert_eq!(
            fs::read(&events).expect("events unchanged"),
            b"event-canary"
        );
        assert!(!session.join("transcript.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn export_no_clobber_is_atomic_against_a_destination_creation_race() {
        let output = tempfile::tempdir().expect("output");
        let parent = fs::canonicalize(output.path()).expect("canonical output");
        let destination = parent.join("transcript.md");
        let destination_for_race = destination.clone();
        let result = write_history_export_unix(
            &parent,
            std::ffi::OsStr::new("transcript.md"),
            b"replacement",
            false,
            move || {
                fs::write(&destination_for_race, b"planted").into_diagnostic()?;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(destination).expect("planted output"), b"planted");
        assert!(fs::read_dir(&parent).expect("output entries").all(|entry| {
            !entry
                .expect("output entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".rottweiler-export-")
        }));
    }
}

#[async_trait]
impl server::ServerEngine for DeferredHostedEngine {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    async fn dispatch(
        &self,
        bound_client: ClientId,
        command: ClientCommand,
    ) -> std::result::Result<CommandOutcome, String> {
        self.loaded().await?.dispatch(bound_client, command).await
    }

    async fn subscribe(
        &self,
        bound_client: ClientId,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<EngineEvent, String>>,
        String,
    > {
        self.loaded()
            .await?
            .subscribe(bound_client, session_id, last_seen)
            .await
    }

    async fn complete_shell(
        &self,
        session_id: SessionId,
        shell_id: rw_core::ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> std::result::Result<(), String> {
        self.loaded()
            .await?
            .complete_shell(session_id, shell_id, status, captured_output)
            .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_serve(
    socket: Option<PathBuf>,
    token_file: Option<PathBuf>,
    session: Option<String>,
    workspace: Option<PathBuf>,
    permission_mode: Option<PermissionMode>,
    max_turns: usize,
    model: Option<String>,
    detach: bool,
    add_dirs: Vec<PathBuf>,
    dangerously_trust: bool,
    in_memory_replay_script: Option<PathBuf>,
    record_script_delay_ms: u64,
    wait_for_execution_lease: bool,
) -> Result<()> {
    let storage_root = configuration_root_path()?;
    let paths = resolve_server_paths(socket, token_file, &storage_root)?;
    let session_id = session
        .or_else(|| std::env::var("ROTTWEILER_SESSION_ID").ok())
        .map_or_else(runtime::new_session_id, Ok)?;
    let workspace = workspace.unwrap_or(std::env::current_dir().into_diagnostic()?);
    let workspace_roots = canonical_workspace_roots(&workspace, &add_dirs)?;

    if detach {
        let workspace = workspace_roots[0].clone();
        return spawn_detached_server(
            &paths,
            &session_id,
            &workspace,
            permission_mode,
            max_turns,
            model.as_deref(),
            &workspace_roots[1..],
            dangerously_trust,
            wait_for_execution_lease,
        )
        .await;
    }

    let (_runtime_directory, runtime, listener) =
        create_guarded_server_runtime(paths, Some(&session_id))?;
    let deferred = DeferredHostedEngine::default();
    let state = server::ServerState::new(Arc::new(deferred.clone()), &runtime);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let serve_task = tokio::spawn(server::serve(listener, state, shutdown_rx));
    let preparation: Result<()> = async {
        ensure_configuration_root(&storage_root)?;
        let workspace = workspace_roots[0].clone();
        let provider_mode = if let Some(script) = in_memory_replay_script.as_deref() {
            runtime::HostedProviderMode::DeterministicReplay {
                provider_name: "local-tui-replay".to_owned(),
                scripts: serde_json::from_slice(&fs::read(script).into_diagnostic()?)
                    .into_diagnostic()?,
                event_delay_ms: record_script_delay_ms,
            }
        } else {
            runtime::HostedProviderMode::Live
        };
        let options = host_runtime::CliHostOptions::from_environment(
            workspace_roots,
            dangerously_trust,
            permission_mode,
            max_turns,
            provider_mode,
            wait_for_execution_lease,
        )
        .map_err(|error| miette!(error.to_string()))?;
        let max_sessions = options.config.engine.max_concurrent_sessions;
        let factory = Arc::new(
            host_runtime::CliSessionFactory::new(options)
                .map_err(|error| miette!(error.to_string()))?,
        );
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions,
                ..EngineHostConfig::default()
            },
            factory.clone(),
            factory,
        )
        .map_err(|error| miette!(error.to_string()))?;
        let resume = session_metadata_path(&storage_root, &session_id).is_file();
        host.prepare_session(
            CreateSessionRequest {
                session_id: SessionId(session_id),
                workspace: workspace.display().to_string(),
                model: model.map(rw_core::ModelAlias),
            },
            resume,
        )
        .await
        .map_err(|error| miette!(error.to_string()))?;
        // The transport socket is created before host composition so health
        // probes can distinguish "starting" from "not running". Do not expose
        // the hosted engine as ready until its supervisor-selected session is
        // actually loaded: an early TUI resume can otherwise reserve the same
        // fresh session id and permanently race initial creation.
        deferred.install(server::HostedEngine::new(host)).await;
        Ok(())
    }
    .await;
    match preparation {
        Ok(()) => {}
        Err(error) => {
            let _ = shutdown.send(true);
            serve_task.await.into_diagnostic()??;
            return Err(error);
        }
    }
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown.send(true);
        }
    });
    serve_task.await.into_diagnostic()?
}

fn configuration_root() -> Result<PathBuf> {
    let root = configuration_root_path()?;
    ensure_configuration_root(&root)?;
    Ok(root)
}

fn canonical_workspace_roots(primary: &Path, added: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut roots = vec![fs::canonicalize(primary).into_diagnostic()?];
    for supplied in added {
        let canonical = fs::canonicalize(supplied)
            .map_err(|error| miette!("--add-dir {} is unavailable: {error}", supplied.display()))?;
        if !canonical.is_dir() {
            return Err(miette!(
                "--add-dir {} is not a directory",
                supplied.display()
            ));
        }
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    Ok(roots)
}

fn prompt_for_folder_trust(
    storage_root: &Path,
    roots: &[PathBuf],
    dangerously_trust: bool,
) -> Result<()> {
    use std::io::IsTerminal as _;

    if dangerously_trust {
        eprintln!(
            "warning: --dangerously-trust enables executable project configuration for this process without persisting a decision"
        );
        return Ok(());
    }
    let store = rw_store::trust::FolderTrustStore::new(storage_root.join("trust.json"));
    for root in roots {
        let assessment = store.assess(root).into_diagnostic()?;
        if assessment.project_execution_enabled() {
            continue;
        }
        eprintln!("{}", assessment.render_prompt());
        if std::io::stdin().is_terminal() {
            eprint!("Trust this exact executable inventory? [y/N] ");
            std::io::stderr().flush().into_diagnostic()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).into_diagnostic()?;
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                store.grant(&assessment).into_diagnostic()?;
                eprintln!(
                    "trusted {}; executable project changes require a session restart",
                    assessment.workspace().display()
                );
            } else {
                eprintln!(
                    "project executable configuration remains inert for {}",
                    assessment.workspace().display()
                );
            }
        } else {
            eprintln!(
                "project executable configuration remains inert; use `rw trust grant` interactively or --dangerously-trust in a controlled CI image"
            );
        }
    }
    Ok(())
}

fn run_trust_command(command: TrustCommand) -> Result<()> {
    use std::io::IsTerminal as _;

    let workspace =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
    let store = rw_store::trust::FolderTrustStore::new(loader.trust_store_path().to_path_buf());
    match command {
        TrustCommand::Status => {
            let assessment = store.assess(&workspace).into_diagnostic()?;
            print!("{}", assessment.render_prompt());
        }
        TrustCommand::Grant => {
            let assessment = store.assess(&workspace).into_diagnostic()?;
            eprint!("{}", assessment.render_prompt());
            if !std::io::stdin().is_terminal() {
                return Err(miette!(
                    "refusing to grant folder trust without an interactive terminal; use --dangerously-trust only for controlled CI images"
                ));
            }
            eprint!("Trust this exact executable inventory? [y/N] ");
            std::io::stderr().flush().into_diagnostic()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).into_diagnostic()?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                return Err(miette!("folder trust was not granted"));
            }
            store.grant(&assessment).into_diagnostic()?;
            println!(
                "trusted {}; restart active sessions to load executable project configuration",
                assessment.workspace().display()
            );
        }
        TrustCommand::Revoke => {
            store.revoke(&workspace).into_diagnostic()?;
            println!(
                "revoked trust for {}; restart active sessions to unload executable project configuration",
                workspace.display()
            );
        }
    }
    Ok(())
}

fn run_plugin_approval(name: Option<&str>, revoke: bool) -> Result<()> {
    use std::io::IsTerminal as _;

    let workspace =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
    let effective_config = loader.load().into_diagnostic()?;
    let storage_root = loader
        .credentials_path()
        .parent()
        .ok_or_else(|| miette!("configuration root has no parent"))?
        .to_path_buf();
    runtime::initialize_private_storage_root(&storage_root).into_diagnostic()?;
    let (user_home, _) = runtime::extension_user_roots(&loader.credentials_path());
    let catalog = m8_config::discover_executable_configs(
        &user_home,
        &workspace,
        effective_config.project_trusted(),
    )?;
    let store = m8_runtime::PrivatePluginApprovalStore::open(&storage_root)?;
    let selected = catalog
        .plugins
        .iter()
        .filter(|plugin| name.is_none_or(|name| plugin.name == name))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(miette!("configured plugin was not found"));
    }
    for plugin in selected {
        if revoke {
            println!(
                "{}",
                if store.revoke(&plugin.name)? {
                    format!("revoked plugin {}", plugin.name)
                } else {
                    format!("plugin {} was not approved", plugin.name)
                }
            );
            continue;
        }
        let manifest = plugin.load_manifest()?;
        let process = plugin.process_config()?;
        let scope = match plugin.origin {
            m8_config::ExecutableConfigOrigin::User(_) => "user",
            m8_config::ExecutableConfigOrigin::TrustedProject(_) => "project",
        };
        let origin = format!("{scope}:{}", plugin.origin.path().display());
        let requirement = rw_core::runtime_support::plugin::plugin_launch_approval_requirement(
            &store, &manifest, &process, &origin,
        )
        .map_err(|error| miette!(error.to_string()))?;
        let summary = serde_json::json!({
            "name": plugin.name, "origin": origin, "executable": process.executable(),
            "argv": process.argv().iter().map(|value| value.to_string_lossy()).collect::<Vec<_>>(),
            "cwd": process.cwd(), "environment_names": process.environment_allowlist(),
            "allowed_domains": process.allowed_domains(), "capabilities": manifest.capabilities,
            "attested_files": process.attested_files(),
            "code_root": process.code_root(),
            "approval": format!("{requirement:?}"),
        });
        let rendered = serde_json::to_string_pretty(&summary).into_diagnostic()?;
        if rendered.len() > 128 * 1024 {
            return Err(miette!("plugin approval summary exceeded its size cap"));
        }
        println!("{rendered}");
        if name.is_none() {
            continue;
        }
        if matches!(
            requirement,
            rw_core::runtime_support::plugin::ApprovalRequirement::Approved
        ) {
            println!("plugin {} is already approved", plugin.name);
            continue;
        }
        if !std::io::stdin().is_terminal() {
            return Err(miette!(
                "refusing plugin approval without an interactive terminal"
            ));
        }
        eprint!("Approve this exact plugin identity? [y/N] ");
        std::io::stderr().flush().into_diagnostic()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).into_diagnostic()?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err(miette!("plugin approval was not granted"));
        }
        rw_core::runtime_support::plugin::approve_plugin_launch(
            &store, &manifest, &process, &origin,
        )
        .map_err(|error| miette!(error.to_string()))?;
        println!(
            "approved plugin {}; restart active sessions to launch it",
            plugin.name
        );
    }
    Ok(())
}

fn configuration_root_path() -> Result<PathBuf> {
    let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
    let root = loader
        .credentials_path()
        .parent()
        .ok_or_else(|| miette!("configuration root has no parent"))?
        .to_path_buf();
    Ok(root)
}

fn ensure_configuration_root(root: &Path) -> Result<()> {
    fs::create_dir_all(root).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).into_diagnostic()?;
    }
    Ok(())
}

struct RuntimeDirectoryGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
    owner: u32,
    armed: bool,
}

fn create_guarded_server_runtime(
    paths: server::ServerRuntimePaths,
    session_id: Option<&str>,
) -> Result<(
    RuntimeDirectoryGuard,
    server::ServerRuntime,
    std::os::unix::net::UnixListener,
)> {
    // Selected remote paths may not exist on first attach. Let the server's
    // owner/private path validation create the leaf before capturing its exact
    // identity for lifecycle cleanup.
    let (runtime, listener) = server::ServerRuntime::create_for_session(paths, session_id)?;
    let runtime_directory = RuntimeDirectoryGuard::capture(&runtime.paths.directory)?;
    Ok((runtime_directory, runtime, listener))
}

impl RuntimeDirectoryGuard {
    fn capture(path: &Path) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let metadata = fs::symlink_metadata(path).into_diagnostic()?;
        let owner = rustix::process::geteuid().as_raw();
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != owner
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return Err(miette!(
                "runtime directory is not one owner-private directory"
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            owner,
            armed: true,
        })
    }

    fn preserve(&mut self) {
        self.armed = false;
    }

    fn validate_identity(&self) -> io::Result<()> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let metadata = fs::symlink_metadata(&self.path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != self.owner
            || metadata.permissions().mode() & 0o777 != 0o700
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime directory identity changed before cleanup",
            ));
        }
        Ok(())
    }

    fn cleanup(&mut self) -> io::Result<()> {
        use std::os::unix::fs::FileTypeExt as _;

        if !self.armed {
            return Ok(());
        }
        if matches!(
            fs::symlink_metadata(&self.path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ) {
            // A supervised serve child may have already removed the exact
            // shared runtime leaf during its own orderly shutdown.
            self.armed = false;
            return Ok(());
        }
        self.validate_identity()?;
        let entries = fs::read_dir(&self.path)?.collect::<io::Result<Vec<_>>>()?;
        for entry in entries {
            let name = entry.file_name();
            let metadata = fs::symlink_metadata(entry.path())?;
            let expected_type = if name == "engine.sock" {
                metadata.file_type().is_socket()
            } else if matches!(
                name.to_str(),
                Some("auth.token" | "runtime.json" | "last-seen")
            ) {
                metadata.is_file() && !metadata.file_type().is_symlink()
            } else {
                false
            };
            if !expected_type {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runtime directory contains an unexpected artifact",
                ));
            }
            self.validate_identity()?;
            fs::remove_file(entry.path())?;
        }
        self.validate_identity()?;
        fs::remove_dir(&self.path)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for RuntimeDirectoryGuard {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            tracing::warn!(
                path = %self.path.display(),
                reason = %error,
                "left runtime directory in place because safe cleanup could not be proven"
            );
        }
    }
}

fn allocate_runtime_paths(root: &Path) -> Result<server::ServerRuntimePaths> {
    fs::create_dir_all(root).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).into_diagnostic()?;
    }
    for _ in 0..32 {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).into_diagnostic()?;
        let suffix = random
            .iter()
            .fold(String::with_capacity(16), |mut value, byte| {
                use std::fmt::Write as _;
                let _ = write!(&mut value, "{byte:02x}");
                value
            });
        let directory = root.join(format!("engine-{suffix}"));
        match fs::create_dir(&directory) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                        .into_diagnostic()?;
                }
                return Ok(server::ServerRuntimePaths {
                    socket: directory.join("engine.sock"),
                    token: directory.join("auth.token"),
                    descriptor: directory.join("runtime.json"),
                    directory,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).into_diagnostic(),
        }
    }
    Err(miette!("could not allocate an engine runtime directory"))
}

fn resolve_server_paths(
    socket: Option<PathBuf>,
    token_file: Option<PathBuf>,
    storage_root: &Path,
) -> Result<server::ServerRuntimePaths> {
    let socket = socket.or_else(|| std::env::var_os("ROTTWEILER_ENGINE_SOCKET").map(PathBuf::from));
    if let Some(socket) = socket {
        let directory = socket
            .parent()
            .ok_or_else(|| miette!("engine socket has no parent directory"))?
            .to_path_buf();
        let token = token_file
            .or_else(|| std::env::var_os("ROTTWEILER_ENGINE_TOKEN_FILE").map(PathBuf::from))
            .unwrap_or_else(|| directory.join("auth.token"));
        return Ok(server::ServerRuntimePaths {
            socket,
            token,
            descriptor: directory.join("runtime.json"),
            directory,
        });
    }
    if token_file.is_some() {
        return Err(miette!("--token-file requires --socket"));
    }
    allocate_runtime_paths(&storage_root.join("run"))
}

fn locate_tui_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().into_diagnostic()?;
    let development =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/tui/dist/rottweiler-tui");
    resolve_tui_executable(
        &current,
        std::env::var_os("ROTTWEILER_TUI_BIN").map(PathBuf::from),
        &development,
    )
}

fn resolve_tui_executable(
    current_executable: &Path,
    override_path: Option<PathBuf>,
    development_path: &Path,
) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return require_executable(path);
    }
    // Package managers expose a public launcher through a symlink or exec
    // wrapper while keeping the complete runtime in a private directory.
    // Resolve the executable that is actually running before looking for its
    // TUI sibling; never derive a helper path from an untrusted PATH entry.
    let installed = fs::canonicalize(current_executable).into_diagnostic()?;
    if let Some(sibling) = installed
        .parent()
        .map(|parent| parent.join("rottweiler-tui"))
        && sibling.is_file()
    {
        return require_executable(sibling);
    }
    require_executable(development_path.to_owned())
}

fn require_executable(path: PathBuf) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(&path).into_diagnostic()?;
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = true;
    if metadata.file_type().is_symlink() || !metadata.is_file() || !executable {
        return Err(miette!(
            "compiled OpenTUI executable is not a regular executable at {}; run `bun run build` in packages/tui or set ROTTWEILER_TUI_BIN",
            path.display()
        ));
    }
    Ok(path)
}

fn session_metadata_path(storage_root: &Path, session_id: &str) -> PathBuf {
    storage_root
        .join("sessions")
        .join(session_id)
        .join("metadata.json")
}

#[allow(clippy::too_many_arguments)]
async fn spawn_detached_server(
    paths: &server::ServerRuntimePaths,
    session_id: &str,
    workspace: &Path,
    permission_mode: Option<PermissionMode>,
    max_turns: usize,
    model: Option<&str>,
    additional_workspaces: &[PathBuf],
    dangerously_trust: bool,
    wait_for_execution_lease: bool,
) -> Result<()> {
    use std::process::Stdio;

    if runtime_is_live(paths).await {
        let token = read_private_bootstrap_token(&paths.token)?
            .ok_or_else(|| miette!("live engine bootstrap token failed validation"))?;
        println!(
            "{}",
            serde_json::json!({
                "version": 1,
                "socket": paths.socket,
                "token": token,
                "session_id": session_id,
                "started": false,
            })
        );
        return Ok(());
    }
    let mut command = tokio::process::Command::new(std::env::current_exe().into_diagnostic()?);
    command
        .arg("serve")
        .arg("--socket")
        .arg(&paths.socket)
        .arg("--token-file")
        .arg(&paths.token)
        .arg("--session")
        .arg(session_id)
        .arg("--workspace")
        .arg(workspace)
        .arg("--max-turns")
        .arg(max_turns.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(mode) = permission_mode {
        command.arg("--permission-mode").arg(mode.as_cli_value());
    }
    if let Some(model) = model {
        command.arg("--model").arg(model);
    }
    for root in additional_workspaces {
        command.arg("--add-dir").arg(root);
    }
    if dangerously_trust {
        command.arg("--dangerously-trust");
    }
    append_execution_lease_restart_flag(&mut command, wait_for_execution_lease);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    let mut child = command.spawn().into_diagnostic()?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if runtime_artifacts_ready(paths) {
            let token = read_private_bootstrap_token(&paths.token)?
                .ok_or_else(|| miette!("new engine bootstrap token failed validation"))?;
            println!(
                "{}",
                serde_json::json!({
                    "version": 1,
                    "socket": paths.socket,
                    "token": token,
                    "session_id": session_id,
                    "started": true,
                })
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait().into_diagnostic()? {
            return Err(miette!(
                "detached engine exited before becoming ready with status {status}"
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            return Err(miette!(
                "detached engine did not become ready within 5 seconds"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

fn append_execution_lease_restart_flag(
    command: &mut tokio::process::Command,
    wait_for_execution_lease: bool,
) {
    if wait_for_execution_lease {
        command.arg("--wait-for-execution-lease");
    }
}

async fn runtime_is_live(paths: &server::ServerRuntimePaths) -> bool {
    if !runtime_artifacts_ready(paths) {
        return false;
    }
    let Ok(Some(token)) = read_private_bootstrap_token(&paths.token) else {
        return false;
    };
    remote::probe_authenticated_health(&paths.socket, &token, std::time::Duration::from_millis(500))
        .await
        .unwrap_or(false)
}

fn runtime_artifacts_ready(paths: &server::ServerRuntimePaths) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        let socket_ready = fs::symlink_metadata(&paths.socket).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink() && metadata.file_type().is_socket()
        });
        let token_ready = fs::symlink_metadata(&paths.token).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink() && metadata.is_file() && metadata.len() == 64
        });
        socket_ready && token_ready
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[allow(clippy::too_many_lines)]
async fn run_remote_tui(host: &str, cli: &Cli) -> Result<()> {
    if cli.continue_latest {
        return Err(miette!(
            "--continue is ambiguous for a remote host; use --resume <session> or the session picker"
        ));
    }
    let local_workspace =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let remote_workspace = cli.remote_workspace.clone().unwrap_or(local_workspace);
    let session_id = cli
        .resume
        .clone()
        .map_or_else(runtime::new_session_id, Ok)?;
    let storage_root = configuration_root()?;
    let local_paths = allocate_runtime_paths(&storage_root.join("run"))?;
    let _runtime_directory = RuntimeDirectoryGuard::capture(&local_paths.directory)?;
    let uid = rustix::process::geteuid().as_raw();
    let session_key = blake3::hash(session_id.as_bytes()).to_hex();
    let remote_socket = PathBuf::from(format!(
        "/tmp/rottweiler-{uid}/engine-{}/engine.sock",
        &session_key[..16]
    ));
    let config = remote::RemoteConfig {
        ssh_executable: std::env::var_os("ROTTWEILER_SSH_BIN")
            .map_or_else(|| PathBuf::from("/usr/bin/ssh"), PathBuf::from),
        host: host.to_owned(),
        remote_rw_executable: std::env::var_os("ROTTWEILER_REMOTE_RW")
            .map_or_else(|| PathBuf::from("/usr/local/bin/rw"), PathBuf::from),
        remote_socket,
        local_socket: local_paths.socket.clone(),
        session_id: session_id.clone(),
        remote_workspace,
        additional_workspaces: cli.add_dirs.clone(),
        dangerously_trust: cli.dangerously_trust,
        model: cli.model.clone(),
        permission_mode: cli.permission_mode.map(|mode| match mode {
            PermissionMode::Strict => remote::RemotePermissionMode::Strict,
            PermissionMode::AutoSafe => remote::RemotePermissionMode::AutoSafe,
            PermissionMode::Yolo => remote::RemotePermissionMode::Yolo,
        }),
    };
    let tui_executable = locate_tui_executable()?;
    let fork_operation_directory = storage_root.join("control/pending-forks");
    let (user_home, user_rottweiler) =
        runtime::extension_user_roots(&storage_root.join("credentials.toml"));
    // Validate all fallible local-only TUI setup before starting a detached
    // remote engine, so invalid user configuration cannot create an orphan.
    let tui_keybindings = tui_config::load_keybindings(None, None, &user_home, &user_rottweiler)
        .map_err(|error| miette!(error.to_string()))?;
    let mut remote_runtime = TokioRemoteRecoveryRuntime::new(config.clone(), local_paths.clone());
    let owned_engine = remote_runtime.ownership();
    if let Err(error) = remote::initialize_remote(&mut remote_runtime).await {
        if !cli.detach
            && let Some(attachment) = error
                .attachment
                .as_ref()
                .filter(|attachment| attachment.started)
            && let Err(shutdown_error) =
                shutdown_remote_using_runtime(&mut remote_runtime, &attachment.bootstrap_token)
                    .await
        {
            tracing::warn!(reason = %shutdown_error, "failed to roll back owned remote startup");
        }
        return Err(miette!(error.message));
    }
    let (watchdog_control, watchdog_commands) = tokio::sync::mpsc::channel(2);
    let mut watchdog = tokio::spawn(remote::run_controlled_watchdog(
        remote_runtime,
        watchdog_commands,
        remote::WatchdogPolicy::default(),
    ));
    let (broker_ready, broker_ready_rx) = tokio::sync::oneshot::channel();
    let mut broker = tokio::spawn(shell_broker::run(
        shell_broker::ShellBrokerConfig {
            socket: local_paths.socket.clone(),
            token_file: local_paths.token.clone(),
            session_id: SessionId(session_id.clone()),
            target: shell_broker::ShellTarget::Remote {
                host: host.to_owned(),
            },
        },
        broker_ready,
    ));
    let broker_readiness = tokio::select! {
        readiness = broker_ready_rx => match readiness {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(miette!(error)),
            Err(error) => Err(error).into_diagnostic(),
        },
        result = &mut watchdog => {
            broker.abort();
            match result {
                Ok(Ok(())) => Err(miette!("remote connection watchdog stopped before broker readiness")),
                Ok(Err(error)) => Err(miette!(error)),
                Err(error) => Err(miette!(error.to_string())),
            }
        }
    };
    if let Err(error) = broker_readiness {
        broker.abort();
        let remote_shutdown = finish_remote_watchdog(
            &watchdog_control,
            &mut watchdog,
            &config,
            &local_paths,
            (!cli.detach).then_some(owned_engine.as_ref()),
        )
        .await;
        if let Err(shutdown_error) = remote_shutdown {
            tracing::warn!(reason = %shutdown_error, "attached remote cleanup also failed");
        }
        return Err(error);
    }
    let tui = run_remote_tui_process(
        tui_executable,
        &local_paths,
        &fork_operation_directory,
        &session_id,
        tui_keybindings.as_deref(),
    );
    tokio::pin!(tui);
    let result = tokio::select! {
        result = &mut tui => result,
        result = &mut broker => match result {
            Ok(Ok(())) => Err(miette!("foreground-shell broker stopped unexpectedly")),
            Ok(Err(error)) => Err(miette!(error.to_string())),
            Err(error) => Err(miette!(error.to_string())),
        },
        result = &mut watchdog => match result {
            Ok(Ok(())) => Err(miette!("remote connection watchdog stopped unexpectedly")),
            Ok(Err(error)) => Err(miette!(error)),
            Err(error) => Err(miette!(error.to_string())),
        },
    };
    broker.abort();
    let remote_shutdown = finish_remote_watchdog(
        &watchdog_control,
        &mut watchdog,
        &config,
        &local_paths,
        (!cli.detach).then_some(owned_engine.as_ref()),
    )
    .await;
    match (result, remote_shutdown) {
        (Err(error), Err(shutdown_error)) => {
            tracing::warn!(reason = %shutdown_error, "attached remote cleanup also failed");
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), shutdown) => shutdown,
    }
}

async fn pause_remote_watchdog(
    watchdog_control: &tokio::sync::mpsc::Sender<remote::WatchdogCommand>,
) -> Result<()> {
    let (acknowledged, paused) = tokio::sync::oneshot::channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(25),
        watchdog_control.send(remote::WatchdogCommand::Pause(acknowledged)),
    )
    .await
    .map_err(|_| miette!("remote watchdog pause timed out before attached shutdown"))?
    .map_err(|_| miette!("remote watchdog stopped before attached shutdown"))?;
    tokio::time::timeout(std::time::Duration::from_secs(25), paused)
        .await
        .map_err(|_| miette!("remote watchdog pause acknowledgement timed out"))?
        .map_err(|_| miette!("remote watchdog stopped before acknowledging attached shutdown"))?;
    Ok(())
}

async fn finish_remote_watchdog(
    watchdog_control: &tokio::sync::mpsc::Sender<remote::WatchdogCommand>,
    watchdog: &mut tokio::task::JoinHandle<std::result::Result<(), String>>,
    config: &remote::RemoteConfig,
    paths: &server::ServerRuntimePaths,
    shutdown_if_owned: Option<&AtomicBool>,
) -> Result<()> {
    let pause = if shutdown_if_owned.is_some() && !watchdog.is_finished() {
        pause_remote_watchdog(watchdog_control).await
    } else {
        Ok(())
    };
    // Load ownership only after recovery is quiescent. A watchdog pass may
    // replace a dead user-owned engine with one created by this invocation.
    let shutdown_owned_engine =
        shutdown_if_owned.is_some_and(|owned_engine| owned_engine.load(Ordering::Acquire));
    let direct_shutdown = if shutdown_owned_engine && pause.is_ok() && !watchdog.is_finished() {
        shutdown_authenticated_remote(paths).await
    } else if shutdown_owned_engine {
        Err(miette!(
            "remote watchdog tunnel is unavailable for attached shutdown"
        ))
    } else {
        Ok(())
    };

    if !watchdog.is_finished() {
        let _ = watchdog_control
            .send(remote::WatchdogCommand::Shutdown)
            .await;
        if tokio::time::timeout(std::time::Duration::from_secs(2), &mut *watchdog)
            .await
            .is_err()
        {
            watchdog.abort();
            let _ = watchdog.await;
        }
    }

    if shutdown_owned_engine && direct_shutdown.is_err() {
        shutdown_remote_with_fresh_tunnel(config, paths).await
    } else {
        direct_shutdown
    }
}

async fn shutdown_authenticated_remote(paths: &server::ServerRuntimePaths) -> Result<()> {
    let token = read_private_bootstrap_token(&paths.token)?
        .ok_or_else(|| miette!("remote engine token disappeared before attached shutdown"))?;
    remote::shutdown_authenticated_host(&paths.socket, &token, std::time::Duration::from_secs(5))
        .await
        .map_err(|error| miette!(error))
}

async fn shutdown_remote_using_runtime(
    runtime: &mut TokioRemoteRecoveryRuntime,
    bootstrap_token: &str,
) -> Result<()> {
    let direct = remote::shutdown_authenticated_host(
        &runtime.paths.socket,
        bootstrap_token,
        std::time::Duration::from_secs(5),
    )
    .await;
    if direct.is_err() {
        remote::RemoteRecoveryRuntime::restart_tunnel(runtime)
            .await
            .map_err(|error| miette!(error))?;
    }
    let result = if direct.is_ok() {
        Ok(())
    } else {
        remote::shutdown_authenticated_host(
            &runtime.paths.socket,
            bootstrap_token,
            std::time::Duration::from_secs(5),
        )
        .await
        .map_err(|error| miette!(error))
    };
    runtime.stop_tunnel().await;
    result
}

async fn shutdown_remote_with_fresh_tunnel(
    config: &remote::RemoteConfig,
    paths: &server::ServerRuntimePaths,
) -> Result<()> {
    let token = read_private_bootstrap_token(&paths.token)?
        .ok_or_else(|| miette!("remote engine token disappeared before fallback shutdown"))?;
    let mut runtime = TokioRemoteRecoveryRuntime::new(config.clone(), paths.clone());
    shutdown_remote_using_runtime(&mut runtime, &token).await
}

struct TokioRemoteRecoveryRuntime {
    config: remote::RemoteConfig,
    paths: server::ServerRuntimePaths,
    tunnel: Option<tokio::process::Child>,
    owned_engine: Arc<AtomicBool>,
}

impl TokioRemoteRecoveryRuntime {
    const HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

    fn new(config: remote::RemoteConfig, paths: server::ServerRuntimePaths) -> Self {
        Self {
            config,
            paths,
            tunnel: None,
            owned_engine: Arc::new(AtomicBool::new(false)),
        }
    }

    fn ownership(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.owned_engine)
    }

    async fn stop_tunnel(&mut self) {
        if let Some(mut child) = self.tunnel.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

#[async_trait]
impl remote::RemoteRecoveryRuntime for TokioRemoteRecoveryRuntime {
    async fn authenticated_health(&mut self) -> std::result::Result<bool, String> {
        let Some(token) =
            read_private_bootstrap_token(&self.paths.token).map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        match remote::probe_authenticated_health(&self.paths.socket, &token, Self::HEALTH_TIMEOUT)
            .await
        {
            Ok(healthy) => Ok(healthy),
            Err(error) => {
                tracing::debug!(reason = %error, "forwarded remote engine health probe failed");
                Ok(false)
            }
        }
    }

    async fn tunnel_alive(&mut self) -> std::result::Result<bool, String> {
        let Some(tunnel) = self.tunnel.as_mut() else {
            return Ok(false);
        };
        let exited = tunnel
            .try_wait()
            .map_err(|error| format!("could not inspect SSH forwarding process: {error}"))?
            .is_some();
        if exited {
            self.tunnel = None;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    async fn restart_tunnel(&mut self) -> std::result::Result<(), String> {
        use std::process::Stdio;

        self.stop_tunnel().await;
        remove_stale_forward_socket(&self.paths.socket)?;
        let forward = self
            .config
            .forward_command()
            .map_err(|error| error.to_string())?;
        let mut command = tokio::process::Command::new(&forward.program);
        command
            .args(&forward.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().process_group(0);
        }
        let mut tunnel = command
            .spawn()
            .map_err(|error| format!("could not start SSH socket forwarding: {error}"))?;
        if let Err(error) = wait_for_socket_or_child(&self.paths.socket, &mut tunnel).await {
            let _ = tunnel.kill().await;
            let _ = tunnel.wait().await;
            return Err(error.to_string());
        }
        self.tunnel = Some(tunnel);
        Ok(())
    }

    async fn attach_or_start(
        &mut self,
        wait_for_execution_lease: bool,
    ) -> std::result::Result<remote::RemoteAttachment, String> {
        use std::process::Stdio;

        let start = if wait_for_execution_lease {
            self.config.engine_recovery_command()
        } else {
            self.config.engine_start_command()
        }
        .map_err(|error| error.to_string())?;
        let mut command = tokio::process::Command::new(&start.program);
        command
            .args(&start.args)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
            .await
            .map_err(|_| "remote attach-or-start command timed out".to_owned())?
            .map_err(|error| format!("could not run remote attach-or-start command: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "remote engine attach-or-start failed with SSH status {}",
                output.status
            ));
        }
        let ready: DetachedServerReady = serde_json::from_slice(&output.stdout)
            .map_err(|_| "remote engine returned an invalid readiness descriptor".to_owned())?;
        if ready.version != 1
            || ready.session_id != self.config.session_id
            || !valid_bootstrap_token(&ready.token)
        {
            return Err("remote engine readiness descriptor failed validation".to_owned());
        }
        if ready.started {
            self.owned_engine.store(true, Ordering::Release);
        }
        Ok(remote::RemoteAttachment {
            bootstrap_token: ready.token,
            started: ready.started,
        })
    }

    async fn install_bootstrap_token(&mut self, token: &str) -> std::result::Result<(), String> {
        if !valid_bootstrap_token(token) {
            return Err("refusing to install invalid remote bootstrap token".to_owned());
        }
        write_private_file_atomic(&self.paths.token, token.as_bytes())
            .map_err(|error| error.to_string())
    }
}

#[derive(serde::Deserialize)]
struct DetachedServerReady {
    version: u16,
    token: String,
    session_id: String,
    #[serde(default)]
    started: bool,
}

fn valid_bootstrap_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read_private_bootstrap_token(path: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(miette!(
            "remote bootstrap-token file is not private and regular"
        ));
    }
    let token = fs::read_to_string(path).into_diagnostic()?;
    let token = token.trim();
    if valid_bootstrap_token(token) {
        Ok(Some(token.to_owned()))
    } else {
        Ok(None)
    }
}

fn write_private_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(miette!(
                    "refusing to replace an unsafe remote bootstrap-token file"
                ));
            }
            if fs::read(path).into_diagnostic()? == bytes {
                return Ok(());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).into_diagnostic(),
    }

    let parent = path
        .parent()
        .ok_or_else(|| miette!("remote bootstrap-token file has no parent"))?;
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).into_diagnostic()?;
    let suffix = random.iter().fold(String::new(), |mut output, byte| {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
        output
    });
    let temporary = parent.join(format!(".auth.token.{suffix}.tmp"));

    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()?;
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).into_diagnostic();
    }
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .into_diagnostic()
}

fn remove_stale_forward_socket(path: &Path) -> std::result::Result<(), String> {
    use std::os::unix::fs::FileTypeExt as _;

    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.file_type().is_socket() => {
            fs::remove_file(path)
                .map_err(|error| format!("could not remove stale forwarded socket: {error}"))
        }
        Ok(_) => Err("refusing to replace an unexpected forwarded-socket artifact".to_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not inspect forwarded socket: {error}")),
    }
}

async fn wait_for_socket_or_child(socket: &Path, child: &mut tokio::process::Child) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if fs::symlink_metadata(socket).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().into_diagnostic()? {
            return Err(miette!(
                "SSH socket forwarding exited before becoming ready with status {status}"
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(miette!(
                "SSH socket forwarding did not become ready within 5 seconds"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

async fn run_remote_tui_process(
    tui: PathBuf,
    paths: &server::ServerRuntimePaths,
    fork_operation_directory: &Path,
    session_id: &str,
    keybindings: Option<&str>,
) -> Result<()> {
    use std::process::Stdio;

    let cursor = paths.directory.join("last-seen");
    for attempt in 0..=5_u8 {
        let mut command = tokio::process::Command::new(&tui);
        command
            .env_remove("ROTTWEILER_TUI_KEYBINDINGS")
            .env("ROTTWEILER_ENGINE_SOCKET", &paths.socket)
            .env("ROTTWEILER_ENGINE_TOKEN_FILE", &paths.token)
            .env("ROTTWEILER_SESSION_ID", session_id)
            .env("ROTTWEILER_LAST_SEEN_FILE", &cursor)
            .env(
                "ROTTWEILER_FORK_OPERATION_DIRECTORY",
                fork_operation_directory,
            )
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(keybindings) = keybindings {
            command.env("ROTTWEILER_TUI_KEYBINDINGS", keybindings);
        }
        let mut child = command.spawn().into_diagnostic()?;
        let status = tokio::select! {
            status = child.wait() => status.into_diagnostic()?,
            interrupted = wait_for_remote_shutdown_signal() => {
                interrupted.into_diagnostic()?;
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(());
            }
        };
        if status.success() {
            return Ok(());
        }
        if attempt == 5 {
            return Err(miette!("remote TUI restart budget exhausted"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(
            50_u64.saturating_mul(1_u64 << attempt),
        ))
        .await;
    }
    Err(miette!("remote TUI stopped unexpectedly"))
}

#[cfg(unix)]
async fn wait_for_remote_shutdown_signal() -> io::Result<()> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    tokio::select! {
        _ = interrupt.recv() => Ok(()),
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn wait_for_remote_shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}

fn auth_set_key(provider_name: &str) -> Result<()> {
    let input = rpassword::prompt_password("API key: ").into_diagnostic()?;
    let api_key = ProviderApiKey::from_terminal_input(input).into_diagnostic()?;
    let warnings = store_provider_api_key(provider_name, api_key).into_diagnostic()?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    println!("stored API key for provider {provider_name}");
    Ok(())
}

async fn auth_login(provider_name: &str) -> Result<()> {
    match begin_provider_login(provider_name)
        .await
        .into_diagnostic()?
    {
        ProviderLogin::OAuth(login) => auth_oauth_login(*login).await,
        ProviderLogin::GitHubCopilot(login) => auth_github_copilot_login(*login).await,
    }
}

async fn auth_oauth_login(login: OAuthLogin) -> Result<()> {
    for warning in login.warnings() {
        eprintln!("warning: {warning}");
    }
    println!("{}", login.authorization_url());
    io::stdout().flush().into_diagnostic()?;
    eprintln!("waiting for OAuth callback on {}", login.redirect_uri());
    let result = login.complete().await.into_diagnostic()?;
    for warning in &result.warnings {
        eprintln!("warning: {warning}");
    }
    if result.refresh_token_stored {
        println!(
            "authenticated provider {}; access and refresh credentials were stored",
            result.provider
        );
    } else {
        eprintln!(
            "warning: provider {} did not issue a refresh token; only the access credential was stored",
            result.provider
        );
        println!("authenticated provider {}", result.provider);
    }
    Ok(())
}

async fn auth_github_copilot_login(login: GitHubCopilotLogin) -> Result<()> {
    for warning in login.warnings() {
        eprintln!("warning: {warning}");
    }
    write_github_device_prompt(
        &mut io::stdout(),
        login.verification_uri(),
        login.user_code(),
    )
    .into_diagnostic()?;
    eprintln!("waiting for GitHub device authorization; press Ctrl-C to cancel");
    let cancellation = ProviderLoginCancellation::default();
    let poll_cancellation = cancellation.clone();
    let result = tokio::select! {
        result = login.complete(&poll_cancellation) => result.into_diagnostic()?,
        signal = tokio::signal::ctrl_c() => {
            signal.into_diagnostic()?;
            cancellation.cancel();
            return Err(miette::miette!("GitHub device authorization cancelled"));
        }
    };
    for warning in &result.warnings {
        eprintln!("warning: {warning}");
    }
    println!("authenticated provider {}", result.provider);
    Ok(())
}

fn write_github_device_prompt(
    writer: &mut impl Write,
    verification_uri: &str,
    user_code: &str,
) -> io::Result<()> {
    writeln!(writer, "Open {verification_uri}")?;
    writeln!(writer, "Enter code: {user_code}")?;
    writer.flush()
}

fn sync_install_paths(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        let descriptor = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| miette!("installer durability path could not be opened safely"))?;
        let stat = rustix::fs::fstat(&descriptor)
            .map_err(|_| miette!("installer durability metadata could not be read"))?;
        let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
        if !kind.is_file() && !kind.is_dir() {
            return Err(miette!(
                "installer durability path must be a regular file or directory"
            ));
        }
        rustix::fs::fsync(&descriptor).map_err(|_| miette!("installer durability flush failed"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{
        Cli, Command, DetachedServerReady, RuntimeDirectoryGuard, TrustCommand, UpgradeChannel,
        append_execution_lease_restart_flag, create_guarded_server_runtime, resolve_tui_executable,
        sync_install_paths, valid_bootstrap_token, write_github_device_prompt,
        write_private_file_atomic,
    };
    #[cfg(unix)]
    use super::{rustix_device_id, rustix_mode_bits};

    #[test]
    fn detached_recovery_threads_the_execution_lease_wait_flag_to_the_real_child() {
        let mut command = tokio::process::Command::new("rw");
        command.arg("serve");
        append_execution_lease_restart_flag(&mut command, true);
        assert!(
            command
                .as_std()
                .get_args()
                .any(|argument| argument == "--wait-for-execution-lease")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_identity_helpers_preserve_signed_failure_and_lossless_mode_widening() {
        assert_eq!(rustix_device_id(-1_i32), None);
        assert_eq!(rustix_device_id(41_i32), Some(41));
        assert_eq!(rustix_device_id(42_u64), Some(42));
        assert_eq!(rustix_mode_bits(0o755_u16), 0o755);
        assert_eq!(rustix_mode_bits(0o700_u32), 0o700);
    }

    #[test]
    fn detached_readiness_ownership_is_explicit_and_old_descriptors_are_conservative()
    -> Result<(), serde_json::Error> {
        let started: DetachedServerReady = serde_json::from_value(serde_json::json!({
            "version": 1,
            "token": "a".repeat(64),
            "session_id": "session",
            "started": true,
        }))?;
        assert!(started.started);

        let pre_existing: DetachedServerReady = serde_json::from_value(serde_json::json!({
            "version": 1,
            "token": "b".repeat(64),
            "session_id": "session",
        }))?;
        assert!(!pre_existing.started);
        Ok(())
    }

    #[test]
    fn trust_and_multi_root_flags_are_global_and_typed() {
        let cli = Cli::try_parse_from([
            "rw",
            "--add-dir",
            "/work/second",
            "--dangerously-trust",
            "trust",
            "status",
        ])
        .unwrap_or_else(|error| panic!("CLI should parse: {error}"));
        assert_eq!(cli.add_dirs, [std::path::PathBuf::from("/work/second")]);
        assert!(cli.dangerously_trust);
        assert!(matches!(
            cli.command,
            Some(Command::Trust {
                command: TrustCommand::Status
            })
        ));
    }

    #[test]
    fn stats_accepts_session_utc_range_and_json_output() {
        let cli = Cli::try_parse_from([
            "rw",
            "stats",
            "--session",
            "session-1",
            "--from",
            "2026-07-01",
            "--to",
            "2026-07-31",
            "--json",
        ])
        .unwrap_or_else(|error| panic!("stats CLI should parse: {error}"));
        assert!(matches!(
            cli.command,
            Some(Command::Stats {
                session: Some(ref session),
                from: Some(ref from),
                through: Some(ref through),
                json: true,
            }) if session == "session-1" && from == "2026-07-01" && through == "2026-07-31"
        ));
    }

    #[test]
    fn doctor_network_probe_is_explicit_and_bounded() {
        let cli =
            Cli::try_parse_from(["rw", "doctor", "--network", "--timeout-ms", "750", "--json"])
                .unwrap_or_else(|error| panic!("doctor CLI should parse: {error}"));
        assert!(matches!(
            cli.command,
            Some(Command::Doctor {
                network: true,
                timeout_ms: 750,
                json: true,
            })
        ));
    }

    #[test]
    fn upgrade_channel_and_downgrade_policy_are_explicit() {
        let cli = Cli::try_parse_from([
            "rw",
            "upgrade",
            "--channel",
            "beta",
            "--allow-downgrade",
            "--timeout-ms",
            "5000",
        ])
        .unwrap_or_else(|error| panic!("upgrade CLI should parse: {error}"));
        assert!(matches!(
            cli.command,
            Some(Command::Upgrade {
                channel: Some(UpgradeChannel::Beta),
                allow_downgrade: true,
                rollback: false,
                timeout_ms: 5_000,
            })
        ));
        assert!(Cli::try_parse_from(["rw", "update"]).is_err());
    }

    #[test]
    fn installer_sync_flushes_files_and_directories_without_following_links() {
        let root = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("sync root should be created: {error}"));
        let file = root.path().join("runtime");
        std::fs::write(&file, b"runtime")
            .unwrap_or_else(|error| panic!("runtime fixture should be written: {error}"));
        sync_install_paths(&[file.clone(), root.path().to_owned()])
            .unwrap_or_else(|error| panic!("durability sync should succeed: {error}"));
        let link = root.path().join("runtime-link");
        std::os::unix::fs::symlink(&file, &link)
            .unwrap_or_else(|error| panic!("link fixture should be created: {error}"));
        assert!(sync_install_paths(&[link]).is_err());
    }

    #[test]
    fn tui_resolution_follows_public_launcher_to_private_runtime_sibling() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
        let private = root.path().join("Cellar/rottweiler/1.2.3/libexec");
        let public = root.path().join("bin");
        std::fs::create_dir_all(&private)
            .unwrap_or_else(|error| panic!("private runtime must exist: {error}"));
        std::fs::create_dir_all(&public)
            .unwrap_or_else(|error| panic!("public bin must exist: {error}"));
        let rw = private.join("rw");
        let tui = private.join("rottweiler-tui");
        for executable in [&rw, &tui] {
            std::fs::write(executable, b"fixture")
                .unwrap_or_else(|error| panic!("executable fixture must exist: {error}"));
            std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))
                .unwrap_or_else(|error| panic!("fixture must be executable: {error}"));
        }
        let launcher = public.join("rw");
        symlink(&rw, &launcher)
            .unwrap_or_else(|error| panic!("public launcher symlink must exist: {error}"));

        assert_eq!(
            resolve_tui_executable(&launcher, None, &root.path().join("missing"))
                .unwrap_or_else(|error| panic!("TUI sibling must resolve: {error}")),
            std::fs::canonicalize(&tui)
                .unwrap_or_else(|error| panic!("TUI sibling must canonicalize: {error}"))
        );

        let override_path = root.path().join("test-tui");
        std::fs::write(&override_path, b"override")
            .unwrap_or_else(|error| panic!("override fixture must exist: {error}"));
        std::fs::set_permissions(&override_path, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("override must be executable: {error}"));
        assert_eq!(
            resolve_tui_executable(&launcher, Some(override_path.clone()), &tui)
                .unwrap_or_else(|error| panic!("explicit override must win: {error}")),
            override_path
        );
    }

    #[test]
    fn owned_runtime_cleanup_removes_only_known_private_artifacts() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
        let runtime = root.path().join("engine-fixture");
        std::fs::create_dir(&runtime)
            .unwrap_or_else(|error| panic!("runtime directory must exist: {error}"));
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("runtime directory must be private: {error}"));
        for name in ["auth.token", "runtime.json", "last-seen"] {
            std::fs::write(runtime.join(name), b"fixture")
                .unwrap_or_else(|error| panic!("runtime artifact must exist: {error}"));
        }
        let listener = std::os::unix::net::UnixListener::bind(runtime.join("engine.sock"))
            .unwrap_or_else(|error| panic!("runtime socket must bind: {error}"));
        let mut guard = RuntimeDirectoryGuard::capture(&runtime)
            .unwrap_or_else(|error| panic!("runtime guard must capture: {error}"));
        drop(listener);
        guard
            .cleanup()
            .unwrap_or_else(|error| panic!("known runtime artifacts must clean: {error}"));
        assert!(!runtime.exists());
    }

    #[test]
    fn guarded_server_creates_a_missing_selected_runtime_before_capture() {
        let root = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
        let directory = root.path().join("remote/engine-fixture");
        let paths = crate::server::ServerRuntimePaths {
            socket: directory.join("engine.sock"),
            token: directory.join("auth.token"),
            descriptor: directory.join("runtime.json"),
            directory: directory.clone(),
        };
        let (mut guard, runtime, listener) =
            create_guarded_server_runtime(paths, Some("remote-session"))
                .unwrap_or_else(|error| panic!("missing selected runtime must start: {error}"));
        assert!(directory.is_dir());
        drop(listener);
        drop(runtime);
        guard
            .cleanup()
            .unwrap_or_else(|error| panic!("created runtime must clean: {error}"));
        assert!(!directory.exists());
    }

    #[test]
    fn owned_runtime_cleanup_refuses_unexpected_or_replaced_directory() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
        let runtime = root.path().join("engine-fixture");
        std::fs::create_dir(&runtime)
            .unwrap_or_else(|error| panic!("runtime directory must exist: {error}"));
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("runtime directory must be private: {error}"));
        std::fs::write(runtime.join("unexpected"), b"keep")
            .unwrap_or_else(|error| panic!("unexpected fixture must exist: {error}"));
        let mut unexpected = RuntimeDirectoryGuard::capture(&runtime)
            .unwrap_or_else(|error| panic!("runtime guard must capture: {error}"));
        assert!(unexpected.cleanup().is_err());
        unexpected.preserve();
        assert!(runtime.join("unexpected").is_file());

        let replacement = root.path().join("engine-replacement");
        std::fs::create_dir(&replacement)
            .unwrap_or_else(|error| panic!("replacement directory must exist: {error}"));
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("replacement directory must be private: {error}"));
        let mut replaced = RuntimeDirectoryGuard::capture(&replacement)
            .unwrap_or_else(|error| panic!("replacement guard must capture: {error}"));
        let moved = root.path().join("moved-original");
        std::fs::rename(&replacement, &moved)
            .unwrap_or_else(|error| panic!("runtime directory must move: {error}"));
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside)
            .unwrap_or_else(|error| panic!("outside directory must exist: {error}"));
        std::fs::write(outside.join("keep"), b"unchanged")
            .unwrap_or_else(|error| panic!("outside fixture must exist: {error}"));
        symlink(&outside, &replacement)
            .unwrap_or_else(|error| panic!("replacement symlink must exist: {error}"));
        assert!(replaced.cleanup().is_err());
        replaced.preserve();
        assert_eq!(
            std::fs::read(outside.join("keep"))
                .unwrap_or_else(|error| panic!("outside fixture must remain: {error}")),
            b"unchanged"
        );
    }

    #[test]
    fn copilot_device_prompt_surfaces_only_the_user_facing_values() {
        let mut output = Vec::new();
        write_github_device_prompt(&mut output, "https://github.com/login/device", "ABCD-EFGH")
            .unwrap_or_else(|error| panic!("device prompt must render: {error}"));
        assert_eq!(
            String::from_utf8(output)
                .unwrap_or_else(|error| panic!("device prompt must be UTF-8: {error}")),
            "Open https://github.com/login/device\nEnter code: ABCD-EFGH\n"
        );
    }

    #[test]
    fn remote_bootstrap_token_rotation_is_atomic_private_and_idempotent() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
        let path = directory.path().join("auth.token");
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        write_private_file_atomic(&path, first.as_bytes())
            .unwrap_or_else(|error| panic!("first token install must succeed: {error}"));
        write_private_file_atomic(&path, first.as_bytes())
            .unwrap_or_else(|error| panic!("same token install must be idempotent: {error}"));
        write_private_file_atomic(&path, second.as_bytes())
            .unwrap_or_else(|error| panic!("token rotation must succeed: {error}"));

        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("token must be readable: {error}")),
            second
        );
        let mode = std::fs::metadata(&path)
            .unwrap_or_else(|error| panic!("token metadata must exist: {error}"))
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
        assert!(valid_bootstrap_token(&first));
        assert!(!valid_bootstrap_token("not-a-token"));
    }

    #[test]
    fn remote_bootstrap_rotation_refuses_symlink_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
        let outside = directory.path().join("outside");
        std::fs::write(&outside, "unchanged")
            .unwrap_or_else(|error| panic!("outside fixture must exist: {error}"));
        let path = directory.path().join("auth.token");
        symlink(&outside, &path)
            .unwrap_or_else(|error| panic!("symlink fixture must exist: {error}"));

        let error = match write_private_file_atomic(&path, "c".repeat(64).as_bytes()) {
            Ok(()) => panic!("symlink token must be refused"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unsafe remote bootstrap-token"));
        assert_eq!(
            std::fs::read_to_string(outside)
                .unwrap_or_else(|read_error| panic!("outside must remain readable: {read_error}")),
            "unchanged"
        );
    }
}
