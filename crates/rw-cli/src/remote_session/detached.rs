//! A new engine remains owned until its validated readiness is delivered.
use super::DetachedServerReady;
use crate::{runtime_paths, server::ServerRuntimePaths, tui_session::Stop};
use miette::{IntoDiagnostic as _, Result, miette};
use std::{future::Future, time::Duration};
use tokio::{process::Child, time::Instant};

const READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ANNOUNCEMENT_BYTES: usize = 64 * 1024;

struct PendingEngine(Option<Child>);
impl PendingEngine {
    async fn settle(&mut self) -> Result<()> {
        if let Some(child) = &mut self.0 {
            // An already-exited child may reject the signal; wait still proves
            // physical retirement and reaps it before any error is returned.
            let _ = child.start_kill();
            child.wait().await.into_diagnostic()?;
        }
        self.0 = None;
        Ok(())
    }
    fn transfer(&mut self) {
        // Command's kill_on_drop is false. Dropping its process handle
        // intentionally leaves the announced engine running.
        self.0 = None;
    }
}
impl Drop for PendingEngine {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            // Panic/runtime destruction cannot await, but must not leave an
            // unannounced engine alive. Normal cancellation uses settle().
            let _ = child.start_kill();
        }
    }
}

pub(super) async fn start<F: Future<Output = Result<()>>>(
    mut command: tokio::process::Command,
    paths: ServerRuntimePaths,
    session_id: String,
    mut stop: Stop,
    deliver: impl FnOnce(DetachedServerReady, Stop) -> F,
) -> Result<()> {
    let already_live = tokio::select! {
        biased;
        () = stop.cancelled() => return Err(miette!("detached engine startup cancelled")),
        live = runtime_paths::runtime_is_live(&paths) => live,
    };
    let mut pending = PendingEngine(None);
    let result = async {
        if !already_live {
            if stop.requested() {
                return Err(miette!("detached engine startup cancelled"));
            }
            // The task owns this handle through readiness and its output write.
            // Only transfer() is permitted to leave the process alive.
            pending.0 = Some(command.kill_on_drop(false).spawn().into_diagnostic()?);
            wait_ready(&mut pending, &paths, &mut stop).await?;
        }
        let token = runtime_paths::read_private_bootstrap_token(&paths.token)?
            .ok_or_else(|| miette!("engine bootstrap token failed validation"))?;
        deliver(
            DetachedServerReady {
                version: 1,
                socket: paths.socket,
                token,
                session_id,
                started: !already_live,
            },
            stop,
        )
        .await
    }
    .await;
    if let Err(error) = result {
        pending.settle().await.map_err(|cleanup| {
            miette!("{error}; detached engine retirement could not be established: {cleanup}")
        })?;
        return Err(error);
    }
    pending.transfer();
    Ok(())
}

async fn wait_ready(
    pending: &mut PendingEngine,
    paths: &ServerRuntimePaths,
    stop: &mut Stop,
) -> Result<()> {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let Some(child) = &mut pending.0 else {
        return Err(miette!("detached startup has no owned engine"));
    };
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return Err(miette!("detached engine startup cancelled")),
            () = tokio::time::sleep_until(deadline) => {
                return Err(miette!("detached engine did not become ready within 5 seconds"));
            }
            status = child.wait() => {
                return Err(miette!("detached engine exited before becoming ready with status {}", status.into_diagnostic()?));
            }
            live = runtime_paths::runtime_is_live(paths) => {
                if live {
                    return Ok(());
                }
            }
        }
        tokio::select! {
            biased;
            () = stop.cancelled() => return Err(miette!("detached engine startup cancelled")),
            () = tokio::time::sleep(Duration::from_millis(2)) => {},
        }
    }
}

pub(super) async fn announce(ready: DetachedServerReady, stop: Stop) -> Result<()> {
    let mut bytes = Vec::new();
    rw_types::json_encoding::JsonWriter::buffer(&mut bytes, MAX_ANNOUNCEMENT_BYTES, 1024)
        .into_diagnostic()?
        .serialize(&ready)
        .into_diagnostic()?;
    if bytes.len() == MAX_ANNOUNCEMENT_BYTES {
        return Err(miette!(
            "detached readiness exceeds its output byte allowance"
        ));
    }
    bytes.push(b'\n');
    crate::headless::announce(String::from_utf8(bytes).into_diagnostic()?, stop).await
}

#[cfg(test)]
mod tests;
