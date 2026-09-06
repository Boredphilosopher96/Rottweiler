//! Session-owned helper authority, captured only at a physical launch boundary.
use std::sync::{Arc, OnceLock};

#[derive(Default)]
pub(crate) struct SandboxHelperSource {
    captured: OnceLock<Result<rw_tools::SandboxHelper, String>>,
}

impl SandboxHelperSource {
    pub(crate) fn pending() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Call only from an admitted finite filesystem worker.
    pub(crate) fn capture(&self) -> Result<rw_tools::SandboxHelper, String> {
        self.captured
            .get_or_init(|| {
                crate::plugin_process::helper_executable().map_err(|error| error.to_string())
            })
            .clone()
    }

    pub(crate) async fn capture_owned(self: &Arc<Self>) -> Result<rw_tools::SandboxHelper, String> {
        if let Some(captured) = self.captured.get() {
            return captured.clone();
        }
        let owner = Arc::clone(self);
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            owner.capture()
        })
        .await
        .map_err(|error| error.to_string())?
    }

    #[cfg(test)]
    pub(crate) fn is_captured(&self) -> bool {
        self.captured.get().is_some()
    }
}
