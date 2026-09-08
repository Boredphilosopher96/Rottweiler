//! One physical image-retirement task survives shutdown waiter cancellation.
use super::super::activation::unsettled;
use rw_ext::PluginRpcError;
use std::sync::Arc;
use tokio::sync::watch;

type Proof = Result<(), PluginRpcError>;
pub(super) struct ImageRetirement(watch::Receiver<Option<Proof>>);
impl ImageRetirement {
    pub(super) fn start(images: Arc<rw_tools::ApprovedExecutableImages>) -> Self {
        Self::start_work(images, PhysicalImages::close)
    }
    fn start_work(
        images: Arc<rw_tools::ApprovedExecutableImages>,
        work: impl FnOnce(PhysicalImages) -> Proof + Send + 'static,
    ) -> Self {
        let (finished, completion) = watch::channel(None);
        match images.close_if_empty() {
            Ok(false) => {}
            proof => {
                finished.send_replace(Some(
                    proof
                        .map(|_| ())
                        .map_err(|cause| unsettled(&cause.to_string())),
                ));
                return Self(completion);
            }
        }
        let owner = PhysicalImages(Some(images));
        tokio::spawn(async move {
            let proof =
                rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                    work(owner)
                })
                .await
                .unwrap_or_else(|cause| Err(unsettled(&cause.to_string())));
            finished.send_replace(Some(proof));
        });
        Self(completion)
    }
    pub(super) async fn wait(&self) -> Proof {
        let mut completion = self.0.clone();
        loop {
            if let Some(proof) = completion.borrow_and_update().clone() {
                return proof;
            }
            completion
                .changed()
                .await
                .map_err(|_| unsettled("image retirement owner exited without proof"))?;
        }
    }
}
struct PhysicalImages(Option<Arc<rw_tools::ApprovedExecutableImages>>);
impl PhysicalImages {
    fn close(mut self) -> Proof {
        let result = self
            .0
            .as_ref()
            .ok_or_else(|| unsettled("image retirement owner is absent"))?
            .close()
            .map_err(|cause| unsettled(&cause.to_string()));
        // The blocking close has destroyed cache entries. Any remaining image
        // belongs to a separately retained process/capture and remains charged.
        self.0.take();
        result
    }
}
impl Drop for PhysicalImages {
    fn drop(&mut self) {
        if let Some(images) = self.0.take() {
            // Runtime loss or admission failure cannot refund a cache that has
            // not reached its physical retirement operation.
            let _ = Box::leak(Box::new(images));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::ImageRetirement;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn lost_waiter_cannot_abandon_or_restart_physical_retirement() {
        use futures_util::FutureExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().expect("fixture");
        let path = directory.path().join("source");
        std::fs::write(&path, b"approved").expect("source");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("mode");
        let receipt = rw_tools::ExecutableArtifactIdentity::capture(
            &path.canonicalize().expect("canonical"),
            1024,
        )
        .expect("receipt");
        let images = Arc::new(rw_tools::ApprovedExecutableImages::default());
        drop(images.acquire(&receipt).expect("cached image"));
        let starts = Arc::new(AtomicUsize::new(0));
        let worker_starts = Arc::clone(&starts);
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let retirement = ImageRetirement::start_work(images.clone(), move |owner| {
            worker_starts.fetch_add(1, Ordering::SeqCst);
            let _ = entered.send(());
            released.recv().expect("physical release");
            owner.close()
        });
        ready.await.expect("physical worker started");
        assert!(retirement.wait().now_or_never().is_none(), "no early proof");
        assert!(images.acquire(&receipt).is_err(), "fenced before dispatch");
        release.send(()).expect("release");
        retirement
            .wait()
            .await
            .expect("same physical close completed");
        retirement.wait().await.expect("stored proof");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(images.close_if_empty().expect("physical cache retired"));
    }
}
