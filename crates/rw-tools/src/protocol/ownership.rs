//! Physical ownership shared by protocol and language-server process handles.
use rw_resources::ResourceLease;
use rw_sandbox::{ProxyLifecycle, SandboxHelper, SupervisedEgressProxy};
use std::{io, process::ExitStatus, time::Duration};
use tokio::process::Child;

pub(crate) struct ProcessOwner(Option<Physical>);
struct Physical {
    child: Child,
    group: Option<u32>,
    signal_result: Option<Result<(), String>>,
    proxy_failed: bool,
    _helper: SandboxHelper,
    _credit: ResourceLease,
    proxy: Option<SupervisedEgressProxy>,
    proxy_job: Option<tokio::task::JoinHandle<()>>,
    proxy_proof: Option<ProxyLifecycle>,
}
impl ProcessOwner {
    pub(crate) fn new(
        child: Child,
        helper: SandboxHelper,
        credit: ResourceLease,
        proxy: Option<SupervisedEgressProxy>,
    ) -> Self {
        let group = child.id();
        Self(Some(Physical {
            child,
            group,
            signal_result: None,
            proxy_failed: false,
            _helper: helper,
            _credit: credit,
            proxy_proof: proxy.as_ref().map(SupervisedEgressProxy::lifecycle),
            proxy,
            proxy_job: None,
        }))
    }
    pub(crate) fn child(&mut self) -> io::Result<&mut Child> {
        self.0
            .as_mut()
            .map(|owner| &mut owner.child)
            .ok_or_else(|| io::Error::other("protocol process is already retired"))
    }
    pub(crate) async fn observe_exit(
        &mut self,
        timeout: Duration,
    ) -> io::Result<Option<ExitStatus>> {
        match tokio::time::timeout(timeout, self.child()?.wait()).await {
            Ok(result) => result.map(Some),
            Err(_) => Ok(None),
        }
    }
    pub(crate) async fn settle(&mut self, timeout: Duration) -> io::Result<()> {
        if let Some(owner) = self.0.as_mut() {
            owner.settle(timeout).await?;
            self.0.take();
        }
        Ok(())
    }
}
impl Physical {
    fn signal(&mut self) -> io::Result<()> {
        if let Some(result) = &self.signal_result {
            return result
                .as_ref()
                .map(|()| ())
                .map_err(|error| io::Error::other(error.clone()));
        }
        #[cfg(unix)]
        let group = if let Some(group) = self
            .group
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
                .or_else(|failure| {
                    if failure == rustix::io::Errno::SRCH {
                        Ok(())
                    } else {
                        Err(failure)
                    }
                })
                .map_err(io::Error::from)
        } else {
            Ok(())
        };
        #[cfg(not(unix))]
        let group: io::Result<()> = Ok(());
        // The direct child remains ours even if it changed its process group.
        let child = self.child.start_kill();
        let result = group.and(child);
        self.signal_result = Some(result.as_ref().map(|()| ()).map_err(ToString::to_string));
        result
    }
    async fn settle(&mut self, timeout: Duration) -> io::Result<()> {
        self.signal()?;
        if self.proxy_failed {
            return Err(io::Error::other("protocol proxy cleanup proof failed"));
        }
        tokio::time::timeout(timeout, async {
            self.child.wait().await?;
            #[cfg(unix)]
            if let Some(group) = self
                .group
                .and_then(|id| i32::try_from(id).ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                loop {
                    match rustix::process::test_kill_process_group(group) {
                        Err(rustix::io::Errno::SRCH) => break,
                        Err(error) => return Err(io::Error::from(error)),
                        Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
                    }
                }
            }
            // A caller can drop this wait; the actual proxy job remains in its owner.
            if let Some(proxy) = self.proxy.take() {
                self.proxy_job = Some(tokio::task::spawn_blocking(move || drop(proxy)));
            }
            if let Some(job) = self.proxy_job.as_mut() {
                if let Err(error) = job.await {
                    self.proxy_failed = true;
                    return Err(io::Error::other(error));
                }
                self.proxy_job.take();
            }
            if self
                .proxy_proof
                .as_ref()
                .is_some_and(|proof| !proof.is_stopped())
            {
                return Err(io::Error::other("protocol proxy effects remain unsettled"));
            }
            self.group = None;
            Ok(())
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "protocol process effects did not settle",
            )
        })?
    }
}
impl Drop for ProcessOwner {
    fn drop(&mut self) {
        if let Some(mut physical) = self.0.take() {
            let _ = physical.signal();
            let mut retirement = Retirement(Some(physical));
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Some(owner) = retirement.0.as_mut()
                        && owner.settle(Duration::from_secs(5)).await.is_ok()
                    {
                        retirement.0.take();
                    }
                });
            }
        }
    }
}
struct Retirement(Option<Physical>);
impl Drop for Retirement {
    fn drop(&mut self) {
        if let Some(owner) = self.0.take() {
            // Keep the actual child, helper, proxy worker and credit on missing proof.
            std::mem::forget(owner);
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
