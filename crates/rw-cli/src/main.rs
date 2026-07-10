use std::{
    io::{self, Write},
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, Result};
use rw_core::{DEFAULT_MODEL_CATALOG_URL, begin_oauth_login, refresh_model_catalog};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "rw", version, about = "Rottweiler coding-agent harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
    /// Run a provider's documented OAuth Authorization Code + PKCE flow.
    Login {
        /// User-configured provider name from `[providers.<name>]`.
        provider: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Config {
            command: ConfigCommand::Check { overrides },
        } => {
            let loaded = rw_store::config::ConfigLoader::from_environment()
                .into_diagnostic()?
                .with_cli_overrides(overrides)
                .load()
                .into_diagnostic()?;
            for warning in loaded.warnings() {
                eprintln!("warning: {}", warning.message());
            }
            print!("{}", loaded.render_with_provenance());
        }
        Command::Models {
            command: ModelsCommand::Refresh { source, output },
        } => {
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
        Command::Auth {
            command: AuthCommand::Login { provider },
        } => auth_login(&provider).await?,
    }

    Ok(())
}

async fn auth_login(provider_name: &str) -> Result<()> {
    let login = begin_oauth_login(provider_name).await.into_diagnostic()?;
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
