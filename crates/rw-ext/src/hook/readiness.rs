//! First-use native readiness shares one clock; callbacks keep their own phase.
use super::{HookError, RegisteredHook};
use crate::{PLUGIN_ACTIVATION_SETTLEMENT_TIMEOUT, PluginActivation};
use futures_util::FutureExt as _;
use std::{panic::AssertUnwindSafe, sync::Arc};
use tokio::time::Instant;

#[tracing::instrument(
    target = "rw_performance",
    level = "trace",
    name = "hook.readiness",
    skip_all
)]
pub(super) async fn prepare_one(hook: &RegisteredHook, deadline: Instant) -> Result<(), HookError> {
    if hook.handler.readiness().is_none() {
        return Ok(());
    }
    if Instant::now() >= deadline {
        return Err(HookError::new(
            "readiness_timeout",
            "aggregate native hook readiness deadline elapsed",
        ));
    }
    let mut owner = hook
        .runtime
        .admit(Arc::clone(&hook.handler), deadline)
        .await?;
    let activation = PluginActivation::until(owner.cancellation.clone(), deadline);
    let outcome = {
        let ready = AssertUnwindSafe(async {
            if let Some(readiness) = hook.handler.readiness() {
                readiness.ready(&activation).await
            } else {
                Ok(())
            }
        })
        .catch_unwind();
        tokio::pin!(ready);
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => {
                owner.cancellation.cancel();
                Err(HookError::new("readiness_timeout", "aggregate native hook readiness deadline elapsed"))
            }
            result = &mut ready => result.unwrap_or_else(|_| Err(HookError::new("panic", "hook readiness panicked"))),
        }
    };
    let cleanup = owner.finish().ok_or_else(|| {
        HookError::new(
            "effects_unsettled",
            "native hook readiness has no settlement owner",
        )
    })?;
    match tokio::time::timeout(PLUGIN_ACTIVATION_SETTLEMENT_TIMEOUT, cleanup).await {
        Ok(Ok(())) => outcome,
        Ok(Err(error)) => Err(error),
        Err(_) => {
            hook.runtime.close_admission();
            Err(HookError::new(
                "effects_unsettled",
                "native hook readiness settlement deadline elapsed",
            ))
        }
    }
}
