//! Ordered external printing owns its bytes through the readline consumer and worker.
use super::{InputLine, MAX_REPL_OUTPUT_BYTES};
use miette::{IntoDiagnostic, Result, miette};
use rustyline::{DefaultEditor, error::ReadlineError};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

// Rustyline can paint one String while its one-slot channel retains another and
// a third waits in ExternalPrinter::print. Reserve that complete envelope before
// starting either owner. Its internal channel cannot carry our individual leases.
const PRINT_SCOPE_BYTES: usize = 3 * MAX_REPL_OUTPUT_BYTES;

struct PrinterScope {
    _bytes: OwnedSemaphorePermit,
}

impl PrinterScope {
    fn acquire() -> Result<Arc<Self>> {
        static OUTPUT: OnceLock<Arc<Semaphore>> = OnceLock::new();
        Self::acquire_from(
            OUTPUT
                .get_or_init(|| Arc::new(Semaphore::new(PRINT_SCOPE_BYTES)))
                .clone(),
        )
    }

    fn acquire_from(bytes: Arc<Semaphore>) -> Result<Arc<Self>> {
        let count = u32::try_from(PRINT_SCOPE_BYTES)
            .map_err(|_| miette!("REPL output allowance exceeds byte admission capacity"))?;
        let permit = bytes
            .try_acquire_many_owned(count)
            .map_err(|_| miette!("REPL output is still owned by an active terminal"))?;
        Ok(Arc::new(Self { _bytes: permit }))
    }
}

pub(super) struct OwnedPrinter {
    printer: Option<Box<dyn rustyline::ExternalPrinter + Send>>,
    scope: Arc<PrinterScope>,
}

impl OwnedPrinter {
    fn new(printer: Box<dyn rustyline::ExternalPrinter + Send>, scope: Arc<PrinterScope>) -> Self {
        Self {
            printer: Some(printer),
            scope,
        }
    }

    pub(super) async fn print(&mut self, message: String) -> Result<()> {
        if message.capacity() > MAX_REPL_OUTPUT_BYTES {
            return Err(miette!("REPL output exceeds its retained byte allowance"));
        }
        // Taking the unique printer prevents another job if this future is cancelled.
        let mut printer = self
            .printer
            .take()
            .ok_or_else(|| miette!("REPL printer ownership has already been transferred"))?;
        let scope = self.scope.clone();
        let (returned, outcome, _scope) =
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                let outcome = printer.print(message);
                (printer, outcome, scope)
            })
            .await
            .into_diagnostic()?;
        self.printer = Some(returned);
        outcome.into_diagnostic()
    }
}

// Field order drops the editor and its queued messages before releasing bytes,
// including unwinding from a terminal error.
struct ReadlineOwner {
    editor: DefaultEditor,
    _scope: Arc<PrinterScope>,
}

pub(super) fn spawn_readline(
    history: PathBuf,
) -> Result<(mpsc::UnboundedReceiver<InputLine>, OwnedPrinter)> {
    let scope = PrinterScope::acquire()?;
    let (send, receive) = mpsc::unbounded_channel();
    let mut editor = DefaultEditor::new().into_diagnostic()?;
    let printer = editor.create_external_printer().into_diagnostic()?;
    let _ = editor.load_history(&history);
    let owned = OwnedPrinter::new(Box::new(printer), scope.clone());
    let mut reader = ReadlineOwner {
        editor,
        _scope: scope,
    };
    std::thread::Builder::new()
        .name("rw-readline".to_owned())
        .spawn(move || {
            loop {
                match reader.editor.readline("rw> ") {
                    Ok(line) => {
                        if !line.trim().is_empty() {
                            let _ = reader.editor.add_history_entry(line.as_str());
                            if let Some(parent) = history.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = reader.editor.save_history(&history);
                        }
                        if send.send(InputLine::Line(line)).is_err() {
                            break;
                        }
                    }
                    Err(ReadlineError::Interrupted) => {
                        if send.send(InputLine::Interrupt).is_err() {
                            break;
                        }
                    }
                    Err(ReadlineError::Eof) => {
                        let _ = send.send(InputLine::Eof);
                        break;
                    }
                    Err(error) => {
                        let _ = send.send(InputLine::Error(error.to_string()));
                        break;
                    }
                }
            }
            if let Some(parent) = history.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = reader.editor.save_history(&history);
        })
        .into_diagnostic()?;
    Ok((receive, owned))
}

#[cfg(test)]
mod tests;
