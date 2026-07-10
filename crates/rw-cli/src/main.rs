use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, Result};
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

fn main() -> Result<()> {
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
    }

    Ok(())
}
