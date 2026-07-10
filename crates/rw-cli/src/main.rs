use std::{
    io::{self, Write},
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, Result};
use rw_core::{
    DEFAULT_MODEL_CATALOG_URL, GitHubCopilotLogin, OAuthLogin, ProviderApiKey, ProviderLogin,
    ProviderLoginCancellation, begin_provider_login, refresh_model_catalog, store_provider_api_key,
};
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
        Command::Auth {
            command: AuthCommand::SetKey { provider },
        } => auth_set_key(&provider)?,
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::write_github_device_prompt;

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
}
