//! Language-server launch authority is prepared only for an actual launch.
use super::super::command_execution::PrivateScratch;
use async_trait::async_trait;
use rw_tools::{
    LspError, LspProcessHandle, LspProcessSpawner, LspServerConfig, SandboxedLspSpawner,
    SpawnedLspProcess,
};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};

const MAX_PREPARATION_WAITERS: usize = 16;

pub(super) struct DeferredLspSpawner {
    roots: Arc<[PathBuf]>,
    prepared: Arc<Mutex<Option<Arc<Prepared>>>>,
    waiting: Arc<Semaphore>,
}
struct Prepared {
    spawner: SandboxedLspSpawner,
    scratch: PrivateScratch,
}
impl DeferredLspSpawner {
    pub(super) fn new(roots: &[PathBuf]) -> Self {
        Self {
            roots: roots.into(),
            prepared: Arc::new(Mutex::new(None)),
            waiting: Arc::new(Semaphore::new(MAX_PREPARATION_WAITERS)),
        }
    }

    async fn admission(&self) -> Result<OwnedMutexGuard<Option<Arc<Prepared>>>, LspError> {
        let waiting = Arc::clone(&self.waiting)
            .try_acquire_owned()
            .map_err(|_| io::Error::other("LSP preparation queue is full"))?;
        let slot = tokio::time::timeout(
            Duration::from_secs(30),
            Arc::clone(&self.prepared).lock_owned(),
        )
        .await
        .map_err(|_| io::Error::other("LSP preparation queue deadline exceeded"))?;
        drop(waiting);
        Ok(slot)
    }
}
fn prepare(roots: &[PathBuf], slot: &mut Option<Arc<Prepared>>) -> Result<Arc<Prepared>, LspError> {
    if let Some(prepared) = slot.as_ref() {
        return Ok(Arc::clone(prepared));
    }
    let scratch =
        PrivateScratch::create("lsp").map_err(|error| io::Error::other(error.to_string()))?;
    let helper = crate::plugin_process::helper_executable()?;
    let prepared = Arc::new(Prepared {
        spawner: SandboxedLspSpawner::new(roots, scratch.path(), &helper)?,
        scratch,
    });
    *slot = Some(Arc::clone(&prepared));
    Ok(prepared)
}
#[async_trait]
impl LspProcessSpawner for DeferredLspSpawner {
    async fn spawn(
        &self,
        workspace: &Path,
        server: &LspServerConfig,
    ) -> Result<SpawnedLspProcess, LspError> {
        // Wait without occupying a global worker. The worker owns this guard
        // after dispatch, even if its caller disappears during preparation.
        let mut slot = self.admission().await?;
        let roots = Arc::clone(&self.roots);
        let workspace = workspace.to_path_buf();
        let server = server.clone();
        let runtime = tokio::runtime::Handle::current();
        // Preparation, launch and handle adoption share one physical worker.
        // Losing the caller cannot release scratch while a child still runs.
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            launch(&roots, &mut slot, &workspace, &server, &runtime)
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
    }
}
fn launch(
    roots: &[PathBuf],
    slot: &mut Option<Arc<Prepared>>,
    workspace: &Path,
    server: &LspServerConfig,
    runtime: &tokio::runtime::Handle,
) -> Result<SpawnedLspProcess, LspError> {
    let owner = prepare(roots, slot)?;
    let mut process = runtime.block_on(owner.spawner.spawn(workspace, server))?;
    process.handle = Box::new(OwnedLspHandle(Some(PhysicalLsp {
        handle: process.handle,
        owner,
    })));
    Ok(process)
}

struct PhysicalLsp {
    handle: Box<dyn LspProcessHandle>,
    owner: Arc<Prepared>,
}
struct OwnedLspHandle(Option<PhysicalLsp>);
#[async_trait]
impl LspProcessHandle for OwnedLspHandle {
    fn request_termination(&mut self) -> io::Result<()> {
        self.0
            .as_mut()
            .map_or(Ok(()), |physical| physical.handle.request_termination())
    }

    async fn kill(&mut self) -> io::Result<()> {
        if let Some(physical) = self.0.as_mut() {
            physical.handle.kill().await?;
        }
        self.0.take();
        Ok(())
    }
}
impl Drop for OwnedLspHandle {
    fn drop(&mut self) {
        let Some(mut physical) = self.0.take() else {
            return;
        };
        // A runtime can discard an unpolled retirement task. Request termination
        // now; only proof is delegated, and failed proof keeps every owner.
        if let Err(error) = physical.handle.request_termination() {
            tracing::warn!(%error, "LSP termination request failed; retaining settlement owner");
        }
        let mut retirement = Retirement(Some(physical));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(physical) = retirement.0.as_mut()
                    && physical.handle.kill().await.is_ok()
                {
                    retirement.0.take();
                }
            });
        }
    }
}
struct Retirement(Option<PhysicalLsp>);
impl Drop for Retirement {
    fn drop(&mut self) {
        if let Some(physical) = self.0.take() {
            tracing::error!(scratch = %physical.owner.scratch.path().display(), "LSP launch authority retained without process settlement proof");
            std::mem::forget(physical);
        }
    }
}

#[cfg(test)]
mod admission_tests;
#[cfg(test)]
mod retirement_tests;
#[cfg(test)]
mod tests;
