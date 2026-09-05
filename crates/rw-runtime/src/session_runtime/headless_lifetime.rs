//! Headless execution retains every cleanup owner across errors and caller drop.
use crate::{
    extension_runtime::PluginActivationBudget, provider_admission::DurableProviderAdmission,
    session_resources::RuntimeSessionResources,
};
use futures_util::FutureExt as _;
use std::{future::Future, sync::Arc};

type Proof = Result<(), Arc<str>>;

pub(super) fn own(
    actor: rw_core::SessionHandle,
    plugins: Arc<PluginActivationBudget>,
    wasm: Arc<rw_ext::WasmWorkerPool>,
    admission: Arc<DurableProviderAdmission>,
) -> Arc<RuntimeSessionResources> {
    let retained = (
        actor.clone(),
        plugins.clone(),
        wasm.clone(),
        admission.clone(),
    );
    RuntimeSessionResources::own_cleanup(retained, async move {
        settle(
            async move { actor.close().await.map_err(message) },
            async move { plugins.close().map_err(message) },
            async move { wasm.shutdown().await.map_err(message) },
            async move { admission.shutdown().await.map_err(message) },
        )
        .await
    })
}

async fn settle(
    actor: impl Future<Output = Proof>,
    plugins: impl Future<Output = Proof>,
    wasm: impl Future<Output = Proof>,
    admission: impl Future<Output = Proof>,
) -> Proof {
    let actor = prove("session actor", actor).await;
    let (plugins, wasm, admission) = tokio::join!(
        prove("plugin activation", plugins),
        prove("WASM workers", wasm),
        prove("provider admission", admission),
    );
    actor.and(plugins).and(wasm).and(admission)
}

async fn prove(name: &str, work: impl Future<Output = Proof>) -> Proof {
    std::panic::AssertUnwindSafe(work)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(Arc::from(format!("{name} cleanup panicked before proof"))))
}

fn message(error: impl std::fmt::Display) -> Arc<str> {
    Arc::from(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn actor_error_and_service_panic_cannot_skip_other_service_proofs() {
        let completed = AtomicUsize::new(0);
        let result = settle(
            async { Err(Arc::from("actor proof failed")) },
            async { panic!("plugin cleanup fixture") },
            async {
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            async {
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert_eq!(result, Err(Arc::from("actor proof failed")));
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn independent_services_start_only_after_actor_cleanup_finishes() {
        let finished = AtomicUsize::new(0);
        let check = || async {
            assert_eq!(finished.load(Ordering::SeqCst), 1);
            Ok(())
        };
        assert!(
            settle(
                async {
                    tokio::task::yield_now().await;
                    finished.store(1, Ordering::SeqCst);
                    Ok(())
                },
                check(),
                check(),
                check(),
            )
            .await
            .is_ok()
        );
    }
}
