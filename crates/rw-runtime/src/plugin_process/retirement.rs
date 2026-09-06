//! Caller destruction transfers the actual child and authority into owned retirement.
use super::{PluginChild, SupervisedPluginProcess, proxy_settlement::PluginProxy};
use std::sync::{Arc, Mutex};

pub(super) fn retire_dropped(process: &mut PluginChild) {
    let Some(admission) = process
        .admission
        .get_mut()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        // A proven retired owner must not signal a subsequently reused PID/group.
        return;
    };
    let child = process
        .child
        .get_mut()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let owner = PluginChild {
        settlement: tokio::sync::Mutex::new(()),
        admission: Mutex::new(Some(admission)),
        helper: process.helper.clone(),
        child: Mutex::new(child),
        process_group: Mutex::new(
            process
                .process_group
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        ),
        violation: Arc::clone(&process.violation),
        proxy: std::mem::replace(&mut process.proxy, PluginProxy::new(None)),
    };
    let _ = owner.kill_tree();
    let mut retirement = Retirement(Some(owner));
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            if let Some(owner) = retirement.0.as_ref()
                && owner.settle_effects().await.is_ok()
            {
                retirement.0.take();
            }
        });
    }
}

struct Retirement(Option<PluginChild>);
impl Drop for Retirement {
    fn drop(&mut self) {
        if let Some(owner) = self.0.take() {
            // No executor, failed wait/proxy proof, or dropped cleanup task:
            // retain child, helper bytes, proxy state and charged capacity.
            tracing::error!("plugin process owner quarantined without settlement proof");
            let _ = Box::leak(Box::new(owner));
        }
    }
}
