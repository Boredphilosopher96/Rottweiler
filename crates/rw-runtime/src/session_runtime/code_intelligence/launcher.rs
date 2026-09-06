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
    sync::{Arc, Mutex},
};

pub(super) struct DeferredLspSpawner {
    roots: Arc<[PathBuf]>,
    prepared: Arc<Mutex<Option<Arc<Prepared>>>>,
}
struct Prepared {
    spawner: SandboxedLspSpawner,
    _scratch: PrivateScratch,
}
impl DeferredLspSpawner {
    pub(super) fn new(roots: &[PathBuf]) -> Self {
        Self {
            roots: roots.into(),
            prepared: Arc::new(Mutex::new(None)),
        }
    }
}
fn prepare(
    roots: &[PathBuf],
    slot: &Mutex<Option<Arc<Prepared>>>,
) -> Result<Arc<Prepared>, LspError> {
    let mut slot = slot
        .lock()
        .map_err(|_| io::Error::other("LSP preparation owner poisoned"))?;
    if let Some(prepared) = slot.as_ref() {
        return Ok(Arc::clone(prepared));
    }
    let scratch =
        PrivateScratch::create("lsp").map_err(|error| io::Error::other(error.to_string()))?;
    let helper = crate::plugin_process::helper_executable()?;
    let prepared = Arc::new(Prepared {
        spawner: SandboxedLspSpawner::new(roots, scratch.path(), &helper)?,
        _scratch: scratch,
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
        let roots = Arc::clone(&self.roots);
        let slot = Arc::clone(&self.prepared);
        let workspace = workspace.to_path_buf();
        let server = server.clone();
        let runtime = tokio::runtime::Handle::current();
        // Preparation, launch and handle adoption share one physical worker.
        // Losing the caller cannot release scratch while a child still runs.
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let owner = prepare(&roots, &slot)?;
            let mut process = runtime.block_on(owner.spawner.spawn(&workspace, &server))?;
            process.handle = Box::new(OwnedLspHandle(Some(PhysicalLsp {
                handle: process.handle,
                _owner: owner,
            })));
            Ok(process)
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
    }
}
struct PhysicalLsp {
    handle: Box<dyn LspProcessHandle>,
    _owner: Arc<Prepared>,
}
struct OwnedLspHandle(Option<PhysicalLsp>);
#[async_trait]
impl LspProcessHandle for OwnedLspHandle {
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
        let Some(physical) = self.0.take() else {
            return;
        };
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
            tracing::error!("LSP launch authority retained without process settlement proof");
            std::mem::forget(physical);
        }
    }
}

#[cfg(test)]
mod tests;
