use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

use clap::Parser;
use miette::{IntoDiagnostic, Result, miette};
use rw_core::refresh_model_catalog;
use rw_runtime::{session, session_history};
use rw_tools::maybe_run_sandbox_helper;
use rw_types::PermissionModeDescriptor as PermissionMode;
use tracing_subscriber::EnvFilter;

mod auth_cli;
mod cli_args;
#[cfg(unix)]
mod history_replay;
#[cfg(unix)]
mod interactive;
#[cfg(unix)]
mod remote_session;
#[cfg(unix)]
mod runtime_paths;
#[cfg(test)]
mod tests;
mod trust_cli;
use crate::auth_cli::{auth_login, auth_set_key};
use crate::cli_args::{
    AuthCommand, Cli, Command, ConfigCommand, DEFAULT_MAX_TURNS, ExtensionCommand,
    ExtensionRegistryCommand, McpCommand, McpServerCommand, ModelsCommand, OutputFormat,
    PluginCommand, PromptCommand, SessionsCommand, max_turns, merge_cli_option,
    merge_workspace_roots, output_format, scripted_provider_options, validate_cli_option_scope,
};
#[cfg(unix)]
use crate::history_replay::run_history_replay;
#[cfg(unix)]
use crate::interactive::{run_local_tui, run_serve};
#[cfg(unix)]
use crate::remote_session::run_remote_tui;
#[cfg(unix)]
use crate::runtime_paths::runtime_root;
use crate::trust_cli::{
    canonical_workspace_roots, configuration_root, configuration_root_path, run_plugin_approval,
    run_trust_command,
};

mod doctor;
mod extension_cli;
mod import;
mod mcp_cli;
mod mcp_server;
mod parent_death;
mod plugin_cli;
mod plugin_dev;
#[allow(dead_code)]
mod remote;
#[allow(dead_code)]
mod server;
#[allow(dead_code)]
mod shell_broker;
mod stats;
#[allow(dead_code)]
mod supervisor;
#[allow(dead_code)]
mod tty;
mod tui_config;
mod upgrade;

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    parent_death::arm_from_environment().into_diagnostic()?;
    if maybe_run_sandbox_helper(std::env::args_os()).map_err(|error| miette!(error.to_string()))? {
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr)
        .init();

    let mut cli = Cli::parse();
    validate_cli_option_scope(&cli)?;
    upgrade::show_pending_release_notes();
    if let Some(host) = cli.remote.as_deref() {
        if cli.command.is_some() || cli.prompt.is_some() {
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
            engine,
            scripted_provider,
            model,
            detach,
            socket,
            token_file,
            session,
            workspace,
            wait_for_execution_lease,
        }) => {
            let permission_mode = merge_cli_option(
                "--permission-mode",
                cli.permission_mode,
                engine.permission_mode,
            )?;
            let max_turns = max_turns(cli.max_turns, engine.max_turns)?;
            let model = merge_cli_option("--model", cli.model, model)?;
            let add_dirs = merge_workspace_roots(cli.add_dirs, engine.add_dirs);
            let (in_memory_replay_script, record_script_delay_ms) = scripted_provider_options(
                cli.in_memory_replay_script,
                scripted_provider.in_memory_replay_script,
                cli.record_script_delay_ms,
                scripted_provider.record_script_delay_ms,
            )?;
            run_serve(
                socket,
                token_file,
                session,
                workspace,
                permission_mode,
                max_turns,
                model,
                cli.detach || detach,
                add_dirs,
                cli.dangerously_trust || engine.dangerously_trust,
                in_memory_replay_script,
                record_script_delay_ms,
                wait_for_execution_lease,
            )
            .await?;
        }
        Some(Command::Prompt {
            options,
            command: PromptCommand::Dump { turn },
        }) => {
            let output_format = output_format(cli.output_format, options.output_format)?;
            let permission_mode = merge_cli_option(
                "--permission-mode",
                cli.permission_mode,
                options.engine.permission_mode,
            )?;
            let max_turns = max_turns(cli.max_turns, options.engine.max_turns)?;
            let resume = merge_cli_option("--resume", cli.resume.clone(), options.resume)?;
            let model = merge_cli_option("--model", cli.model, options.model)?;
            session::run(session::RunOptions {
                prompt: None,
                output_format: output_format.into(),
                permission_mode,
                max_turns,
                continue_latest: resume.is_none(),
                resume,
                replay_dir: None,
                record_replay_script: None,
                in_memory_replay_script: None,
                record_script_delay_ms: 0,
                perf_markers: false,
                replay_provider: "prompt-dump-offline".to_owned(),
                model,
                additional_workspaces: merge_workspace_roots(cli.add_dirs, options.engine.add_dirs),
                dangerously_trust: cli.dangerously_trust || options.engine.dangerously_trust,
                action: session::RunAction::PromptDump { turn },
            })
            .await?;
        }
        Some(Command::Config {
            dangerously_trust,
            command: ConfigCommand::Check { overrides },
        }) => {
            let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
            let loader = if cli.dangerously_trust || dangerously_trust {
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
            command: ModelsCommand::List { output, refresh },
        }) => {
            let catalog = session::discover_model_catalog(refresh).await?;
            let output_format = output_format(cli.output_format, output.output_format)?;
            match output_format {
                OutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&catalog).into_diagnostic()?
                ),
                OutputFormat::StreamJson => {
                    println!("{}", serde_json::to_string(&catalog).into_diagnostic()?);
                }
                OutputFormat::Text => {
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
        }
        Some(Command::Models {
            command:
                ModelsCommand::Show {
                    output,
                    id,
                    refresh,
                },
        }) => {
            let catalog = session::discover_model_catalog(refresh).await?;
            let model = catalog
                .models
                .iter()
                .find(|model| model.id == id)
                .ok_or_else(|| miette!("model {id:?} is not present in the live catalog"))?;
            let output_format = output_format(cli.output_format, output.output_format)?;
            if output_format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(model).into_diagnostic()?);
            } else if output_format == OutputFormat::StreamJson {
                println!("{}", serde_json::to_string(model).into_diagnostic()?);
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
            command: PluginCommand::Check { path, allow_exec },
        }) => {
            if !allow_exec {
                return Err(miette!(
                    "plugin check executes local plugin code; pass --allow-exec to grant explicit authority"
                ));
            }
            plugin_cli::check_typescript(&path)?;
            println!("plugin check passed: {}", path.display());
        }
        Some(Command::Plugin {
            command: PluginCommand::Status,
        }) => run_plugin_approval(None, false).await?,
        Some(Command::Plugin {
            command: PluginCommand::Approve { name },
        }) => run_plugin_approval(Some(&name), false).await?,
        Some(Command::Plugin {
            command: PluginCommand::Revoke { name },
        }) => run_plugin_approval(Some(&name), true).await?,
        Some(Command::Plugin {
            command:
                PluginCommand::Dev {
                    path,
                    session,
                    allow_dev_exec,
                },
        }) => {
            if !allow_dev_exec {
                return Err(miette!(
                    "plugin dev executes local code; pass --allow-dev-exec to grant explicit development authority"
                ));
            }
            plugin_dev::run(&path, &session, &runtime_root(&configuration_root_path()?)).await?;
        }
        Some(Command::Extension {
            command:
                ExtensionCommand::Registry {
                    command: ExtensionRegistryCommand::List { catalog },
                },
        }) => extension_cli::list_registry(&catalog).await?,
        Some(Command::Extension {
            command:
                ExtensionCommand::Registry {
                    command:
                        ExtensionRegistryCommand::Install {
                            name,
                            version,
                            catalog,
                            publisher_key,
                        },
                },
        }) => {
            let store = configuration_root()?.join("extensions");
            extension_cli::install_registry_release(
                &store,
                &catalog,
                &name,
                version.as_deref(),
                &publisher_key,
            )
            .await?;
        }
        Some(Command::Extension {
            command: ExtensionCommand::Status,
        }) => extension_cli::status(&configuration_root()?.join("extensions"))?,
        Some(Command::Extension {
            command: ExtensionCommand::Enable { name, version, yes },
        }) => {
            extension_cli::enable(
                &configuration_root()?.join("extensions"),
                &name,
                &version,
                yes,
            )
            .await?;
        }
        Some(Command::Extension {
            command: ExtensionCommand::Disable { name },
        }) => extension_cli::disable(&configuration_root()?.join("extensions"), &name)?,
        Some(Command::McpServer {
            engine,
            scripted_provider,
            command: McpServerCommand::Stdio { workspace },
        }) => {
            let workspace = workspace.unwrap_or(std::env::current_dir().into_diagnostic()?);
            let add_dirs = merge_workspace_roots(cli.add_dirs, engine.add_dirs);
            let workspace_roots = canonical_workspace_roots(&workspace, &add_dirs)?;
            let (in_memory_replay_script, record_script_delay_ms) = scripted_provider_options(
                cli.in_memory_replay_script,
                scripted_provider.in_memory_replay_script,
                cli.record_script_delay_ms,
                scripted_provider.record_script_delay_ms,
            )?;
            let provider_mode = if let Some(script) = in_memory_replay_script.as_deref() {
                session::HostedProviderMode::DeterministicReplay {
                    provider_name: "mcp-server-replay".to_owned(),
                    scripts: serde_json::from_slice(&fs::read(script).into_diagnostic()?)
                        .into_diagnostic()?,
                    event_delay_ms: record_script_delay_ms,
                }
            } else {
                session::HostedProviderMode::Live
            };
            let options = rw_runtime::RuntimeHostOptions::from_environment(
                workspace_roots,
                cli.dangerously_trust || engine.dangerously_trust,
                merge_cli_option(
                    "--permission-mode",
                    cli.permission_mode,
                    engine.permission_mode,
                )?,
                max_turns(cli.max_turns, engine.max_turns)?,
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
            dangerously_trust,
            command: McpCommand::Login { server },
        }) => mcp_cli::login(&server, cli.dangerously_trust || dangerously_trust).await?,
        Some(Command::Replay { session, jsonl }) => {
            let storage_root = configuration_root_path()?;
            if jsonl {
                let events = session_history::load_events(&storage_root, &session)?;
                io::stdout()
                    .write_all(&session_history::replay_jsonl(&events)?)
                    .into_diagnostic()?;
            } else {
                run_history_replay(&storage_root, &session).await?;
            }
        }
        Some(Command::Export {
            session,
            format,
            output,
            force,
        }) => {
            let storage_root = configuration_root_path()?;
            let events = session_history::load_events(&storage_root, &session)?;
            let redactor = rw_providers::FixtureRedactor::default();
            session::register_credential_environment(&redactor);
            let exported =
                session_history::export_transcript(&session, &events, format.into(), &redactor)?;
            if let Some(path) = output {
                session_history::write_transcript_export(&storage_root, &path, &exported, force)?;
            } else {
                io::stdout().write_all(&exported).into_diagnostic()?;
            }
        }
        Some(Command::Sessions {
            output,
            command: SessionsCommand::Verify { session },
        }) => {
            let verified = session_history::verify_session(&configuration_root_path()?, &session)?;
            match output_format(cli.output_format, output.output_format)? {
                OutputFormat::Text => println!(
                    "Verified {}: {} events, {} bytes",
                    verified.session_id, verified.events, verified.bytes
                ),
                OutputFormat::Json | OutputFormat::StreamJson => {
                    println!("{}", serde_json::to_string(&verified).into_diagnostic()?);
                }
            }
        }
        Some(Command::Sessions {
            output,
            command: SessionsCommand::Search { query, limit },
        }) => {
            let sessions =
                session_history::search_sessions(&configuration_root_path()?, &query, limit)?;
            render_session_search(
                &sessions,
                output_format(cli.output_format, output.output_format)?,
            )?;
        }
        Some(Command::Sessions {
            output,
            command: SessionsCommand::List { limit },
        }) => {
            let sessions = session_history::list_sessions(&configuration_root_path()?, limit)?;
            render_session_search(
                &sessions,
                output_format(cli.output_format, output.output_format)?,
            )?;
        }
        Some(Command::Stats {
            output,
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
            let output_format = output_format(cli.output_format, output.output_format)?;
            match (json, output_format) {
                (true, _) | (false, OutputFormat::Json | OutputFormat::StreamJson) => {
                    println!("{}", serde_json::to_string(&report).into_diagnostic()?);
                }
                (false, OutputFormat::Text) => print!("{}", stats::render_text(&report)),
            }
        }
        Some(Command::Import {
            output,
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
            let output_format = output_format(cli.output_format, output.output_format)?;
            match (json, output_format) {
                (true, _) | (false, OutputFormat::Json | OutputFormat::StreamJson) => {
                    println!("{}", serde_json::to_string(&report).into_diagnostic()?);
                }
                (false, OutputFormat::Text) => {
                    for item in report.items {
                        println!("{:?}\t{}\t{}", item.status, item.target, item.detail);
                    }
                }
            }
        }
        Some(Command::Doctor {
            output,
            network,
            timeout_ms,
            json,
        }) => {
            let report = doctor::collect(doctor::DoctorOptions {
                network,
                timeout_ms,
            })
            .await;
            let output_format = output_format(cli.output_format, output.output_format)?;
            match (json, output_format) {
                (true, _) | (false, OutputFormat::Json | OutputFormat::StreamJson) => {
                    println!("{}", serde_json::to_string(&report).into_diagnostic()?);
                }
                (false, OutputFormat::Text) => print!("{}", doctor::render_text(&report)),
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
            let headless = cli.prompt.is_some()
                || cli.replay_dir.is_some()
                || cli.record_replay_script.is_some()
                || cli.perf_markers;
            if headless {
                session::run(session::RunOptions {
                    prompt: cli.prompt,
                    output_format: cli.output_format.unwrap_or_default().into(),
                    permission_mode: cli.permission_mode,
                    max_turns: cli.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
                    resume: cli.resume,
                    continue_latest: cli.continue_latest,
                    replay_dir: cli.replay_dir,
                    record_replay_script: cli.record_replay_script,
                    in_memory_replay_script: cli.in_memory_replay_script,
                    record_script_delay_ms: cli.record_script_delay_ms.unwrap_or_default(),
                    perf_markers: cli.perf_markers,
                    replay_provider: cli
                        .replay_provider
                        .unwrap_or_else(|| "cli-replay".to_owned()),
                    model: cli.model,
                    additional_workspaces: cli.add_dirs,
                    dangerously_trust: cli.dangerously_trust,
                    action: session::RunAction::Agent,
                })
                .await?;
            } else {
                run_local_tui(&cli).await?;
            }
        }
    }

    Ok(())
}

fn render_session_search(
    sessions: &[rw_store::session::SessionSummary],
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Text => {
            print!("{}", render_session_search_text(sessions)?);
        }
        OutputFormat::Json => {
            let values = sessions
                .iter()
                .map(|session| {
                    serde_json::json!({
                        "id":session.id,"title":session.title,
                        "updated_unix_ms":session.updated_unix_ms,"cost_micros":session.cost_micros,
                        "turn_count":session.turn_count,
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
                        "turn_count":session.turn_count,
                    })
                );
            }
        }
    }
    Ok(())
}

fn render_session_search_text(sessions: &[rw_store::session::SessionSummary]) -> Result<String> {
    use std::fmt::Write as _;

    let mut output = String::from("UPDATED (UTC)\tTURNS\tTITLE\tSESSION\n");
    for session in sessions {
        let unix_millis = u64::try_from(session.updated_unix_ms)
            .map_err(|_| miette!("session update time is before the Unix epoch"))?;
        let timestamp = rw_store::session::UtcTimestamp::from_unix_millis(unix_millis)
            .map_err(|error| miette!("session update time is invalid: {error}"))?;
        let updated = timestamp.as_str()[..16].replace('T', " ");
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
        writeln!(
            output,
            "{updated}\t{}\t{title}\t{}",
            session.turn_count, session.id,
        )
        .into_diagnostic()?;
    }
    Ok(output)
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
