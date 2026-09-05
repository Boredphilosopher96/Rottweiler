use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use miette::{Result, miette};
use rw_core::DEFAULT_MODEL_CATALOG_URL;
use rw_types::PermissionModeDescriptor as PermissionMode;

use crate::import;

#[derive(Debug, Parser)]
#[command(name = "rw", version, about = "Rottweiler coding-agent harness")]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct Cli {
    /// Run one prompt without starting the interactive application.
    #[arg(short = 'p', long, value_name = "PROMPT")]
    pub(super) prompt: Option<String>,
    /// Rendering contract for print mode.
    #[arg(long, value_enum)]
    pub(super) output_format: Option<OutputFormat>,
    /// Non-interactive permission policy. Omitted means the loaded config policy.
    #[arg(long)]
    pub(super) permission_mode: Option<PermissionMode>,
    /// Maximum provider iterations permitted in one user turn.
    #[arg(long)]
    pub(super) max_turns: Option<usize>,
    /// Run the `OpenTUI` locally against an engine reached over SSH.
    #[arg(long, value_name = "HOST")]
    pub(super) remote: Option<String>,
    /// Workspace path on the remote engine host; defaults to the local path.
    #[arg(long, value_name = "PATH", requires = "remote")]
    pub(super) remote_workspace: Option<PathBuf>,
    /// Keep the engine alive after the interactive client exits.
    #[arg(long)]
    pub(super) detach: bool,
    /// Add another canonical workspace root for tools and sandbox writes.
    #[arg(long = "add-dir", value_name = "PATH")]
    pub(super) add_dirs: Vec<PathBuf>,
    /// Enable executable project configuration without persisting trust.
    #[arg(long)]
    pub(super) dangerously_trust: bool,
    /// Resume an exact durable session id.
    #[arg(long, value_name = "SESSION", conflicts_with = "continue_latest")]
    pub(super) resume: Option<String>,
    /// Continue the most recently updated durable session.
    #[arg(long = "continue", conflicts_with = "resume")]
    pub(super) continue_latest: bool,
    /// Network-free provider recording directory used by deterministic tests.
    #[arg(long, hide = true, value_name = "DIRECTORY")]
    pub(super) replay_dir: Option<PathBuf>,
    /// Record a deterministic provider-event script for CLI acceptance tests.
    #[arg(long, hide = true, value_name = "SCRIPT", requires = "replay_dir")]
    pub(super) record_replay_script: Option<PathBuf>,
    /// Use a deterministic in-memory provider-event script without fixture I/O.
    #[arg(
        long,
        hide = true,
        value_name = "SCRIPT",
        conflicts_with = "record_replay_script"
    )]
    pub(super) in_memory_replay_script: Option<PathBuf>,
    /// Delay each scripted provider event for crash/interrupt acceptance tests.
    #[arg(long, hide = true)]
    pub(super) record_script_delay_ms: Option<u64>,
    /// Emit deterministic timing markers for the release performance smoke.
    #[arg(long, hide = true)]
    pub(super) perf_markers: bool,
    /// Provider name stored in the deterministic replay directory.
    #[arg(long, hide = true)]
    pub(super) replay_provider: Option<String>,
    /// Override the active provider-neutral model alias.
    #[arg(long, value_name = "ALIAS")]
    pub(super) model: Option<String>,
    #[command(subcommand)]
    pub(super) command: Option<Command>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(super) enum OutputFormat {
    #[default]
    Text,
    Json,
    StreamJson,
}

impl From<OutputFormat> for rw_runtime::OutputFormat {
    fn from(value: OutputFormat) -> Self {
        match value {
            OutputFormat::Text => Self::Text,
            OutputFormat::Json => Self::Json,
            OutputFormat::StreamJson => Self::StreamJson,
        }
    }
}

pub(super) const DEFAULT_MAX_TURNS: usize = 32;

#[derive(Debug, Default, Args)]
pub(super) struct EnginePolicyArgs {
    /// Non-interactive permission policy. Omitted means the loaded config policy.
    #[arg(
        id = "engine_permission_mode",
        long = "permission-mode",
        value_name = "PERMISSION_MODE",
        value_enum,
        global = true
    )]
    pub(super) permission_mode: Option<PermissionMode>,
    /// Maximum provider iterations permitted in one user turn.
    #[arg(
        id = "engine_max_turns",
        long = "max-turns",
        value_name = "MAX_TURNS",
        global = true
    )]
    pub(super) max_turns: Option<usize>,
    /// Add another canonical workspace root for tools and sandbox writes.
    #[arg(
        id = "engine_add_dirs",
        long = "add-dir",
        value_name = "PATH",
        global = true
    )]
    pub(super) add_dirs: Vec<PathBuf>,
    /// Enable executable project configuration without persisting trust.
    #[arg(
        id = "engine_dangerously_trust",
        long = "dangerously-trust",
        global = true
    )]
    pub(super) dangerously_trust: bool,
}

#[derive(Debug, Default, Args)]
pub(super) struct ScriptedProviderArgs {
    /// Use a deterministic in-memory provider-event script without fixture I/O.
    #[arg(
        id = "scripted_in_memory_replay_script",
        long = "in-memory-replay-script",
        hide = true,
        value_name = "SCRIPT",
        global = true
    )]
    pub(super) in_memory_replay_script: Option<PathBuf>,
    /// Delay each scripted provider event for crash/interrupt acceptance tests.
    #[arg(
        id = "scripted_record_script_delay_ms",
        long = "record-script-delay-ms",
        hide = true,
        global = true
    )]
    pub(super) record_script_delay_ms: Option<u64>,
}

#[derive(Debug, Default, Args)]
pub(super) struct PromptOptions {
    /// Rendering contract for the assembled prompt.
    #[arg(
        id = "prompt_output_format",
        long = "output-format",
        value_name = "OUTPUT_FORMAT",
        value_enum,
        global = true
    )]
    pub(super) output_format: Option<OutputFormat>,
    #[command(flatten)]
    pub(super) engine: EnginePolicyArgs,
    /// Resume an exact durable session id.
    #[arg(
        id = "prompt_resume",
        long = "resume",
        value_name = "SESSION",
        global = true
    )]
    pub(super) resume: Option<String>,
    /// Override the active provider-neutral model alias.
    #[arg(
        id = "prompt_model",
        long = "model",
        value_name = "ALIAS",
        global = true
    )]
    pub(super) model: Option<String>,
}

#[derive(Debug, Default, Args)]
pub(super) struct MachineOutputArgs {
    /// Rendering contract for machine-readable output.
    #[arg(
        id = "machine_output_format",
        long = "output-format",
        value_name = "OUTPUT_FORMAT",
        value_enum,
        global = true
    )]
    pub(super) output_format: Option<OutputFormat>,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    /// Internal release-installer durability helper.
    #[command(name = "__install-sync", hide = true)]
    InstallSync {
        /// Exact regular files/directories to flush without following symlinks.
        #[arg(value_name = "PATH", num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Run the authenticated headless engine server.
    Serve {
        #[command(flatten)]
        engine: EnginePolicyArgs,
        #[command(flatten)]
        scripted_provider: ScriptedProviderArgs,
        /// Override the active provider-neutral model alias.
        #[arg(long, value_name = "ALIAS")]
        model: Option<String>,
        /// Keep the engine alive after the interactive client exits.
        #[arg(long)]
        detach: bool,
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
        #[command(flatten)]
        options: PromptOptions,
        #[command(subcommand)]
        command: PromptCommand,
    },
    /// Inspect Rottweiler configuration.
    Config {
        /// Enable executable project configuration without persisting trust.
        #[arg(
            id = "config_dangerously_trust",
            long = "dangerously-trust",
            global = true
        )]
        dangerously_trust: bool,
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
    /// Author, approve, and debug process-isolated plugins.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Install and manage signed WASM hook extensions.
    Extension {
        #[command(subcommand)]
        command: ExtensionCommand,
    },
    /// Expose approved Rottweiler tools and connection-owned sessions over MCP.
    McpServer {
        #[command(flatten)]
        engine: EnginePolicyArgs,
        #[command(flatten)]
        scripted_provider: ScriptedProviderArgs,
        #[command(subcommand)]
        command: McpServerCommand,
    },
    /// Manage configured MCP clients.
    Mcp {
        /// Enable executable project configuration without persisting trust.
        #[arg(
            id = "mcp_dangerously_trust",
            long = "dangerously-trust",
            global = true
        )]
        dangerously_trust: bool,
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
        #[command(flatten)]
        output: MachineOutputArgs,
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// Report bounded historical tokens, costs, cache savings, and tool use.
    Stats {
        #[command(flatten)]
        output: MachineOutputArgs,
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
        #[command(flatten)]
        output: MachineOutputArgs,
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
        #[command(flatten)]
        output: MachineOutputArgs,
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
pub(super) enum UpgradeChannel {
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
pub(super) enum HistoryExportFormat {
    Markdown,
    Html,
    Json,
}

impl From<HistoryExportFormat> for rw_core::TranscriptFormat {
    fn from(value: HistoryExportFormat) -> Self {
        match value {
            HistoryExportFormat::Markdown => Self::Markdown,
            HistoryExportFormat::Html => Self::Html,
            HistoryExportFormat::Json => Self::Json,
        }
    }
}

#[derive(Debug, Subcommand)]
pub(super) enum SessionsCommand {
    /// Verify all persisted segments and event identities in an offline session.
    Verify {
        #[arg(value_name = "SESSION")]
        session: String,
    },
    /// List sessions from newest to oldest.
    List {
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
pub(super) enum McpCommand {
    /// Authenticate one configured HTTP MCP server with Authorization Code + PKCE.
    Login { server: String },
}

#[derive(Debug, Subcommand)]
pub(super) enum McpServerCommand {
    /// Serve one MCP connection over standard input/output.
    Stdio {
        /// Primary workspace exposed to the server; defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum PluginCommand {
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
    /// Validate a TypeScript plugin manifest, types, and behavior tests.
    Check {
        #[arg(value_name = "PATH", default_value = ".")]
        path: PathBuf,
        /// Explicitly authorize the plugin's local typecheck and test commands.
        #[arg(long)]
        allow_exec: bool,
    },
    /// Attach a source plugin to a live local session with hot reload.
    Dev {
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Live session selector (`current` or an exact session ID).
        #[arg(long, default_value = "current")]
        session: String,
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

#[derive(Debug, Subcommand)]
pub(super) enum ExtensionCommand {
    /// Browse or install signed releases from a configured registry URL.
    Registry {
        #[command(subcommand)]
        command: ExtensionRegistryCommand,
    },
    /// Show exact enabled versions and fingerprints.
    Status,
    /// Explicitly enable one installed extension version.
    Enable {
        name: String,
        version: String,
        /// Confirm the displayed exact capability manifest non-interactively.
        #[arg(long)]
        yes: bool,
    },
    /// Disable an extension without deleting installed versions.
    Disable { name: String },
}

#[derive(Debug, Subcommand)]
pub(super) enum ExtensionRegistryCommand {
    /// List releases from a bounded HTTPS registry catalog.
    List {
        #[arg(long, value_name = "HTTPS_URL")]
        catalog: String,
    },
    /// Download and verify a release, leaving it inactive.
    Install {
        name: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, value_name = "HTTPS_URL")]
        catalog: String,
        /// Independently trusted unpadded-base64 Ed25519 publisher key.
        #[arg(long, value_name = "BASE64_KEY")]
        publisher_key: String,
    },
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub(super) enum TrustCommand {
    /// Show the exact project extension inventory and its trust state.
    Status,
    /// Trust the exact currently displayed project extension inventory.
    Grant,
    /// Revoke the current workspace decision.
    Revoke,
}

#[derive(Debug, Subcommand)]
pub(super) enum PromptCommand {
    /// Print the exact provider-neutral request for the latest or selected turn.
    Dump {
        /// Historical agent turn to assemble; omitted selects the latest state.
        #[arg(long, value_name = "N")]
        turn: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigCommand {
    /// Validate and print the effective configuration with provenance.
    Check {
        /// Apply a highest-precedence KEY=VALUE override.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        overrides: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum ModelsCommand {
    /// List cached concrete models; use --refresh for live provider discovery.
    List {
        #[command(flatten)]
        output: MachineOutputArgs,
        /// Contact configured providers and update the private cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Show one exact cached `provider/model` record.
    Show {
        #[command(flatten)]
        output: MachineOutputArgs,
        /// Concrete provider-qualified model id.
        id: String,
        /// Contact configured providers and update the private cache.
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
pub(super) enum AuthCommand {
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

pub(super) fn validate_cli_option_scope(cli: &Cli) -> Result<()> {
    let Some(command) = cli.command.as_ref() else {
        return validate_default_command_options(cli);
    };

    let supports_output = command_supports_output(command);
    let supports_engine_policy = matches!(
        command,
        Command::Serve { .. } | Command::Prompt { .. } | Command::McpServer { .. }
    );
    let supports_workspace_roots = supports_engine_policy;
    let supports_project_trust = matches!(
        command,
        Command::Serve { .. }
            | Command::Prompt { .. }
            | Command::Config { .. }
            | Command::McpServer { .. }
            | Command::Mcp { .. }
    );
    let supports_model = matches!(command, Command::Serve { .. } | Command::Prompt { .. });
    let supports_detach = matches!(command, Command::Serve { .. });
    let supports_resume = matches!(command, Command::Prompt { .. });
    let supports_scripted_provider =
        matches!(command, Command::Serve { .. } | Command::McpServer { .. });

    let mut invalid = Vec::new();
    if cli.prompt.is_some() {
        invalid.push("--prompt");
    }
    if cli.output_format.is_some() && !supports_output {
        invalid.push("--output-format");
    }
    if cli.permission_mode.is_some() && !supports_engine_policy {
        invalid.push("--permission-mode");
    }
    if cli.max_turns.is_some() && !supports_engine_policy {
        invalid.push("--max-turns");
    }
    if cli.remote.is_some() {
        invalid.push("--remote");
    }
    if cli.remote_workspace.is_some() {
        invalid.push("--remote-workspace");
    }
    if cli.detach && !supports_detach {
        invalid.push("--detach");
    }
    if !cli.add_dirs.is_empty() && !supports_workspace_roots {
        invalid.push("--add-dir");
    }
    if cli.dangerously_trust && !supports_project_trust {
        invalid.push("--dangerously-trust");
    }
    if cli.resume.is_some() && !supports_resume {
        invalid.push("--resume");
    }
    if cli.continue_latest {
        invalid.push("--continue");
    }
    if cli.replay_dir.is_some() {
        invalid.push("--replay-dir");
    }
    if cli.record_replay_script.is_some() {
        invalid.push("--record-replay-script");
    }
    if cli.in_memory_replay_script.is_some() && !supports_scripted_provider {
        invalid.push("--in-memory-replay-script");
    }
    if cli.record_script_delay_ms.is_some() && !supports_scripted_provider {
        invalid.push("--record-script-delay-ms");
    }
    if cli.perf_markers {
        invalid.push("--perf-markers");
    }
    if cli.replay_provider.is_some() {
        invalid.push("--replay-provider");
    }
    if cli.model.is_some() && !supports_model {
        invalid.push("--model");
    }

    if invalid.is_empty() {
        Ok(())
    } else {
        Err(miette!(
            "{} cannot be used with this subcommand",
            invalid.join(", ")
        ))
    }
}

pub(super) fn command_supports_output(command: &Command) -> bool {
    matches!(
        command,
        Command::Prompt { .. }
            | Command::Models {
                command: ModelsCommand::List { .. } | ModelsCommand::Show { .. },
            }
            | Command::Sessions { .. }
            | Command::Stats { .. }
            | Command::Import { .. }
            | Command::Doctor { .. }
    )
}

pub(super) fn validate_default_command_options(cli: &Cli) -> Result<()> {
    let headless = cli.prompt.is_some()
        || cli.replay_dir.is_some()
        || cli.record_replay_script.is_some()
        || cli.perf_markers;
    if cli.output_format.is_some() && !headless {
        return Err(miette!(
            "--output-format requires --prompt or a replay/performance run"
        ));
    }
    if cli.replay_provider.is_some()
        && cli.replay_dir.is_none()
        && cli.in_memory_replay_script.is_none()
    {
        return Err(miette!(
            "--replay-provider requires --replay-dir or --in-memory-replay-script"
        ));
    }
    if cli.record_script_delay_ms.is_some()
        && cli.record_replay_script.is_none()
        && cli.in_memory_replay_script.is_none()
    {
        return Err(miette!(
            "--record-script-delay-ms requires a scripted provider"
        ));
    }
    Ok(())
}

pub(super) fn merge_cli_option<T>(
    name: &str,
    root: Option<T>,
    subcommand: Option<T>,
) -> Result<Option<T>> {
    match (root, subcommand) {
        (Some(_), Some(_)) => Err(miette!(
            "{name} was supplied both before and after the subcommand"
        )),
        (Some(value), None) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

pub(super) fn merge_workspace_roots(
    mut root: Vec<PathBuf>,
    subcommand: Vec<PathBuf>,
) -> Vec<PathBuf> {
    root.extend(subcommand);
    root
}

pub(super) fn output_format(
    root: Option<OutputFormat>,
    subcommand: Option<OutputFormat>,
) -> Result<OutputFormat> {
    Ok(merge_cli_option("--output-format", root, subcommand)?.unwrap_or_default())
}

pub(super) fn max_turns(root: Option<usize>, subcommand: Option<usize>) -> Result<usize> {
    Ok(merge_cli_option("--max-turns", root, subcommand)?.unwrap_or(DEFAULT_MAX_TURNS))
}

pub(super) fn scripted_provider_options(
    root_script: Option<PathBuf>,
    subcommand_script: Option<PathBuf>,
    root_delay_ms: Option<u64>,
    subcommand_delay_ms: Option<u64>,
) -> Result<(Option<PathBuf>, u64)> {
    let script = merge_cli_option("--in-memory-replay-script", root_script, subcommand_script)?;
    let delay_ms = merge_cli_option(
        "--record-script-delay-ms",
        root_delay_ms,
        subcommand_delay_ms,
    )?;
    if delay_ms.is_some() && script.is_none() {
        return Err(miette!(
            "--record-script-delay-ms requires --in-memory-replay-script"
        ));
    }
    Ok((script, delay_ms.unwrap_or_default()))
}
