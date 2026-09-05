//! Local CLI client: terminal I/O consumes the runtime command/event surface.
mod output;

use crate::cli_args::OutputFormat;
use miette::{IntoDiagnostic, Result, miette};
use rw_core::TurnStatus;
use rw_runtime::session::{LocalSessionOptions, compose_local_session};

pub(super) struct ClientOptions {
    pub prompt: Option<String>,
    pub format: OutputFormat,
    pub perf_markers: bool,
}

pub(super) async fn run(options: LocalSessionOptions, client: ClientOptions) -> Result<()> {
    let session = compose_local_session(options).await?;
    if client.perf_markers {
        // Composition is complete: provider/tool/command registries, MCP, and actor.
        eprintln!("rw_perf_prompt_ready=1");
    }
    let execution = async {
        if let Some(dump) = session.prompt_dump() {
            serde_json::to_writer_pretty(std::io::stdout().lock(), dump).into_diagnostic()?;
            println!();
            Ok(None)
        } else if let Some(prompt) = client.prompt {
            output::run_print(
                session.handle(),
                session.session_id(),
                &prompt,
                client.format,
                client.perf_markers,
            )
            .await
        } else {
            output::run_repl(session.handle(), session.storage_root(), client.format).await
        }
    }
    .await;
    // Broken pipes, input errors, and rejected dispatches still settle all effects.
    session.close().await?;
    if let Some(status) = execution?
        && status != TurnStatus::Completed
    {
        return Err(miette!("agent turn ended with status {status:?}"));
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn display_agent_error(error: rw_core::AgentLoopError) -> miette::Report {
    miette!(error.to_string())
}
