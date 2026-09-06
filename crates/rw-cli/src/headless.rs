//! Local CLI client: terminal I/O consumes the runtime command/event surface.
mod output;

use crate::cli_args::OutputFormat;
use miette::{Result, miette};
use rw_core::TurnStatus;
use rw_runtime::session::{LocalSessionOptions, compose_local_session};

pub(super) struct ClientOptions {
    pub prompt: Option<String>,
    pub format: OutputFormat,
    pub perf_markers: bool,
}

pub(super) async fn run(options: LocalSessionOptions, client: ClientOptions) -> Result<()> {
    let session = compose_local_session(options).await?;
    let execution = async {
        if let Some(dump) = session.prompt_dump() {
            output::print_dump(session.handle(), dump, client.perf_markers).await
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
            if client.perf_markers {
                eprintln!("rw_perf_prompt_ready=1");
            }
            output::run_repl(session.handle(), client.format).await
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
