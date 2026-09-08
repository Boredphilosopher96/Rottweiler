//! Readiness is transferred only after the physical output owner acknowledges it.
use super::terminal::Terminal;
use crate::tui_session::Stop;
use miette::{Result, miette};

pub(crate) async fn announce(message: String, mut stop: Stop) -> Result<()> {
    let mut terminal = tokio::select! {
        biased;
        () = stop.cancelled() => return Err(miette!("readiness output cancelled")),
        terminal = Terminal::start_output() => terminal?,
    };
    let result = tokio::select! {
        biased;
        () = stop.cancelled() => Err(miette!("readiness output cancelled")),
        result = terminal.print(message) => result,
    };
    // Restore descriptor flags and retire the physical write before the caller
    // can transfer or terminate its process. No stdin or signal owner is opened.
    terminal.close().await?;
    result
}
