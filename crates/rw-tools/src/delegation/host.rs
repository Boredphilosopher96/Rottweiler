//! One bounded nested execution lane inside an authorized tool invocation.
use super::{ToolEffectGrant, ToolEffectScope};
use crate::invocation_effects::{InvocationEffect, InvocationEffects};
use crate::{
    CancellationToken, CapabilityManifest, MutationScope, Tool, ToolContext, ToolError,
    ToolRegistry, ToolResult,
};
use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::Value;
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU16, Ordering},
};
use std::time::Duration;
use tokio::sync::Mutex;

const MAX_EFFECT_CALLS: u16 = 128;
const EFFECT_SETTLEMENT_DEADLINE: Duration = Duration::from_secs(5);

/// Host-owned callbacks supplied only after outer approval/checkpoint capture.
#[async_trait]
pub trait ToolEffectHost: Send + Sync {
    /// # Errors
    /// Rejects authority, admission, cancellation and execution failures.
    async fn call(
        &self,
        grant: &ToolEffectGrant,
        name: &str,
        input: Value,
    ) -> Result<ToolResult, ToolError>;
    /// Closes admission and proves every accepted nested effect has retired.
    async fn close_and_settle(&self) -> Result<(), ToolError>;
}

pub struct DelegatedTools {
    approved: CapabilityManifest,
    checkpoint: MutationScope,
    scope: Arc<std::sync::OnceLock<Result<ToolEffectScope, String>>>,
    context: ToolContext,
    tools: Arc<ToolRegistry>,
    effects: Arc<InvocationEffects>,
    lane: Mutex<()>,
    closed: AtomicBool,
    failed: AtomicBool,
    calls: AtomicU16,
    cancellation: CancellationToken,
}
impl DelegatedTools {
    #[must_use]
    pub fn new(
        context: ToolContext,
        tools: Arc<ToolRegistry>,
        approved: CapabilityManifest,
        checkpoint: MutationScope,
    ) -> Self {
        Self {
            approved,
            checkpoint,
            scope: Arc::new(std::sync::OnceLock::new()),
            context,
            tools,
            effects: Arc::default(),
            lane: Mutex::new(()),
            closed: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            calls: AtomicU16::new(0),
            cancellation: CancellationToken::default(),
        }
    }
}
struct ToolEffect {
    tool: Arc<dyn Tool>,
    authorization: tokio::sync::watch::Receiver<bool>,
}
struct AuthorizationFinished(tokio::sync::watch::Sender<bool>);
impl Drop for AuthorizationFinished {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
#[async_trait]
impl InvocationEffect for ToolEffect {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        let mut authorization = self.authorization.clone();
        authorization
            .wait_for(|finished| *finished)
            .await
            .map_err(|_| {
                ToolError::EffectsUnsettled("nested authorization owner disappeared".into())
            })?;
        self.tool.settle_effects().await
    }
}
#[async_trait]
impl ToolEffectHost for DelegatedTools {
    async fn call(
        &self,
        grant: &ToolEffectGrant,
        name: &str,
        input: Value,
    ) -> Result<ToolResult, ToolError> {
        // There is no pending nested queue behind a mutation checkpoint.
        let _lane = self.lane.try_lock().map_err(|_| {
            ToolError::DelegationDenied(
                "another nested effect is still owned by this invocation".into(),
            )
        })?;
        if self.closed.load(Ordering::Acquire)
            || self.failed.load(Ordering::Acquire)
            || self.cancellation.is_cancelled()
            || self.context.cancellation.is_cancelled()
        {
            return Err(ToolError::DelegationDenied(
                "invocation effect scope is closed".into(),
            ));
        }
        self.calls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_EFFECT_CALLS).then(|| count + 1)
            })
            .map_err(|_| {
                ToolError::DelegationDenied("invocation effect call count exhausted".into())
            })?;
        let tool = self.tools.resolve(name).ok_or_else(|| {
            ToolError::DelegationDenied(
                "requested tool is not in the captured invocation registry".into(),
            )
        })?;
        let (authorized, authorization) = tokio::sync::watch::channel(false);
        let operation = self.effects.begin(
            Arc::new(ToolEffect {
                tool: tool.clone(),
                authorization,
            }),
            self.cancellation.clone(),
        )?;
        // The callback task owns this blocking job until it returns. Canonical
        // path resolution never runs in the actor or RPC reader loop.
        let authorize = {
            let scope = self.scope.clone();
            let context = self.context.clone();
            let approved = self.approved.clone();
            let checkpoint = self.checkpoint.clone();
            let grant = grant.clone();
            let tool = tool.clone();
            let cancellation = self.cancellation.clone();
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                let _finished = AuthorizationFinished(authorized);
                let scope = scope.get_or_init(|| {
                    ToolEffectScope::new(&context, approved, &checkpoint)
                        .map_err(|error| error.to_string())
                });
                let scope = scope
                    .as_ref()
                    .map_err(|message| ToolError::DelegationDenied(message.clone()))?;
                let context = scope
                    .authorize(&context, &grant, tool.as_ref(), &input)?
                    .without_effect_host()
                    .with_cancellation(cancellation);
                Ok::<_, ToolError>((context, input))
            })
            .await
        };
        let (context, input) = match authorize {
            Ok(Ok(authorized)) => authorized,
            Ok(Err(error)) => {
                operation.finish().await?;
                return Err(error);
            }
            Err(error) => {
                self.failed.store(true, Ordering::Release);
                return Err(ToolError::EffectsUnsettled(format!(
                    "nested authorization task failed: {error}"
                )));
            }
        };
        let result = {
            let execute = AssertUnwindSafe(tool.execute(&context, input)).catch_unwind();
            tokio::pin!(execute);
            tokio::select! {
                biased;
                () = self.context.cancellation.cancelled() => {
                    self.cancellation.cancel();
                    Err(ToolError::Cancelled)
                },
                () = self.cancellation.cancelled() => Err(ToolError::Cancelled),
                result = &mut execute => result.unwrap_or_else(|_| Err(ToolError::EffectsUnsettled(
                    "nested tool implementation panicked".into()))),
            }
        };
        if let Err(error) = operation.finish().await {
            self.failed.store(true, Ordering::Release);
            return Err(error);
        }
        if matches!(result, Err(ToolError::EffectsUnsettled(_))) {
            self.failed.store(true, Ordering::Release);
        }
        result
    }
    async fn close_and_settle(&self) -> Result<(), ToolError> {
        self.closed.store(true, Ordering::Release);
        self.cancellation.cancel();
        let proof = async {
            let _lane = self.lane.lock().await;
            self.effects.settle().await?;
            if self.failed.load(Ordering::Acquire) {
                return Err(ToolError::EffectsUnsettled(
                    "nested effect proof failed".into(),
                ));
            }
            Ok(())
        };
        if let Ok(result) = tokio::time::timeout(EFFECT_SETTLEMENT_DEADLINE, proof).await {
            result
        } else {
            self.failed.store(true, Ordering::Release);
            Err(ToolError::EffectsUnsettled(
                "nested effect settlement deadline expired".into(),
            ))
        }
    }
}
