//! Decoded host replies retain shared admission through redaction and RPC encoding.
use super::{PluginBoundaryRedactor, PluginRpcError, rpc_error};
use rw_plugin_protocol::{MAX_FRAME_BYTES, RpcFailure, RpcFrame, RpcId, RpcSuccess};
use rw_types::{
    allocation::{AllocationPlan, PrepareAllocation},
    json_encoding::JsonWriter,
    json_structure::{JsonStructureLimits, preflight_json},
};
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const POOL_BYTES: usize = 64 * 1024 * 1024;
const VALUE_BYTES: usize = 16 * 1024 * 1024;
const FRAME_OVERHEAD: usize = 1024;
/// Required response envelope declared by the typed host handler before execution.
#[derive(Clone, Copy)]
pub struct PushReplyLimits {
    encoded: usize,
    decoded: usize,
}
impl PushReplyLimits {
    /// Scalar command outcomes, status acknowledgements and bounded session headers.
    pub const ACKNOWLEDGEMENT: Self = Self {
        encoded: 16 * 1024,
        decoded: 256 * 1024,
    };
    /// Canonical content and context/state pages.
    pub const CONTENT: Self = Self {
        encoded: MAX_FRAME_BYTES,
        decoded: VALUE_BYTES,
    };
    /// One source-qualified event chunk, including its JSON escaping and metadata.
    pub const EVENT_CHUNK: Self = Self {
        encoded: 1024 * 1024,
        decoded: 8 * 1024 * 1024,
    };
    const fn construction(self) -> usize {
        self.encoded * 4 + self.decoded
    }
}
fn pool() -> Arc<Semaphore> {
    static POOL: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(POOL.get_or_init(|| Arc::new(Semaphore::new(POOL_BYTES))))
}
fn unavailable() -> PluginRpcError {
    rpc_error(
        "reply_admission",
        "host reply exceeds available decoded or encoded admission",
    )
}

/// Acquired before a callback constructs its response or starts delegated effects.
pub struct PushReplySlot {
    credit: OwnedSemaphorePermit,
    limits: PushReplyLimits,
}
impl PushReplySlot {
    pub(super) fn try_acquire(limits: PushReplyLimits) -> Result<Self, PluginRpcError> {
        Self::from_pool(pool(), limits)
    }
    pub(super) fn from_pool(
        pool: Arc<Semaphore>,
        limits: PushReplyLimits,
    ) -> Result<Self, PluginRpcError> {
        let count = u32::try_from(limits.construction()).map_err(|_| unavailable())?;
        Ok(Self {
            limits,
            credit: pool
                .try_acquire_many_owned(count)
                .map_err(|_| unavailable())?,
        })
    }
    /// Serialize and structurally admit a typed response before allocating JSON containers.
    /// The typed source remains owned by the caller until this function returns.
    /// # Errors
    /// Rejects oversized encodings, decoded amplification, or invalid serialization.
    pub fn encode(&mut self, source: &impl Serialize) -> Result<PushReply, PluginRpcError> {
        if self.credit.num_permits() < self.limits.construction() {
            return Err(unavailable());
        }
        let mut encoded = Vec::new();
        JsonWriter::buffer(&mut encoded, self.limits.encoded, 0)
            .map_err(|_| unavailable())?
            .serialize(source)
            .map_err(|_| unavailable())?;
        let shape = preflight_json(
            &encoded,
            JsonStructureLimits {
                max_encoded_bytes: self.limits.encoded,
                max_nodes: 65_536,
                max_string_bytes: self.limits.encoded,
                max_depth: 64,
            },
        )
        .map_err(|_| unavailable())?;
        if shape
            .direct_value_decode_bytes()
            .is_none_or(|bytes| bytes > self.limits.decoded)
        {
            return Err(unavailable());
        }
        let value: Value = serde_json::from_slice(&encoded).map_err(|_| unavailable())?;
        let plan = AllocationPlan::new(value).map_err(|_| unavailable())?;
        if plan.bytes() > self.limits.decoded {
            return Err(unavailable());
        }
        let value = plan.prepare();
        drop(encoded);
        let bytes = value.bytes() + FRAME_OVERHEAD;
        let retained = ReplyRetention {
            credit: self.credit.split(bytes).ok_or_else(unavailable)?,
            source: None,
        };
        Ok(PushReply {
            value: value.into_inner(),
            retained,
            limits: self.limits,
        })
    }
    pub(super) fn failure(self, id: RpcId, error: PluginRpcError) -> OwnedPushFrame {
        // Failure diagnostics also cross an admitted boundary. Invalid unbounded
        // producer errors become an explicit admission failure, never a partial message.
        let error = if error
            .message
            .capacity()
            .saturating_add(error.code.capacity())
            > self.limits.encoded
        {
            unavailable()
        } else {
            error
        };
        OwnedPushFrame {
            frame: RpcFrame::Failure(RpcFailure {
                jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.into(),
                id: Some(id),
                error: rw_plugin_protocol::RpcErrorObject {
                    code: -32000,
                    message: error.message,
                    data: Some(serde_json::json!({"code":error.code})),
                },
            }),
            retained: ReplyRetention {
                credit: self.credit,
                source: None,
            },
        }
    }
}

/// A decoded response with compulsory physical byte ownership, independent of its caller.
pub struct PushReply {
    value: Value,
    retained: ReplyRetention,
    limits: PushReplyLimits,
}
impl PushReply {
    /// Keep an upstream result owner through the RPC writer's encoding handoff.
    #[must_use]
    pub fn retain(mut self, owner: impl Send + Sync + 'static) -> Self {
        self.retained.source = Some(Box::new((self.retained.source.take(), owner)));
        self
    }
    pub(super) fn redact(
        mut self,
        id: RpcId,
        redactor: &dyn PluginBoundaryRedactor,
    ) -> Result<OwnedPushFrame, PluginRpcError> {
        // The original value stays charged; two bounded string replacement buffers
        // may coexist inside the redactor. Never wait while retaining this value.
        let scratch = Arc::clone(self.retained.credit.semaphore())
            .try_acquire_many_owned(
                u32::try_from(2 * self.limits.decoded).map_err(|_| unavailable())?,
            )
            .map_err(|_| unavailable())?;
        self.retained.credit.merge(scratch);
        let mut bytes = self.value.prepared_bytes().ok_or_else(unavailable)?;
        redact_value(&mut self.value, redactor, &mut bytes, self.limits.decoded)?;
        let plan =
            AllocationPlan::new(std::mem::take(&mut self.value)).map_err(|_| unavailable())?;
        if plan.bytes() > self.limits.decoded {
            return Err(unavailable());
        }
        let prepared = plan.prepare();
        let id_bytes = match &id {
            RpcId::String(value) => value.capacity(),
            RpcId::Number(_) => 0,
        };
        let bytes = prepared
            .bytes()
            .checked_add(FRAME_OVERHEAD)
            .and_then(|bytes| bytes.checked_add(id_bytes))
            .ok_or_else(unavailable)?;
        self.value = prepared.into_inner();
        if bytes > self.retained.credit.num_permits() {
            return Err(unavailable());
        }
        self.retained.shrink(bytes);
        Ok(OwnedPushFrame {
            frame: RpcFrame::Success(RpcSuccess {
                jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.into(),
                id,
                result: self.value,
            }),
            retained: self.retained,
        })
    }
}
fn redact_value(
    value: &mut Value,
    redactor: &dyn PluginBoundaryRedactor,
    bytes: &mut usize,
    ceiling: usize,
) -> Result<(), PluginRpcError> {
    match value {
        Value::String(text) => {
            let other = bytes.checked_sub(text.capacity()).ok_or_else(unavailable)?;
            let limit = ceiling.checked_sub(other).ok_or_else(unavailable)?;
            let replacement = redactor.redact_reply_text(text, limit)?;
            *bytes = other
                .checked_add(replacement.capacity())
                .filter(|bytes| *bytes <= ceiling)
                .ok_or_else(unavailable)?;
            *text = replacement;
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value, redactor, bytes, ceiling)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_value(value, redactor, bytes, ceiling)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}
pub(super) struct ReplyRetention {
    credit: OwnedSemaphorePermit,
    source: Option<Box<dyn Send + Sync>>,
}
impl ReplyRetention {
    fn shrink(&mut self, bytes: usize) {
        if bytes < self.credit.num_permits() {
            drop(self.credit.split(self.credit.num_permits() - bytes));
        }
    }
}
pub(super) struct OwnedPushFrame {
    pub(super) frame: RpcFrame,
    pub(super) retained: ReplyRetention,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    #[test]
    fn construction_admission_precedes_callback_and_refunds_after_rejection() {
        let pool = Arc::new(Semaphore::new(POOL_BYTES));
        let first =
            PushReplySlot::from_pool(pool.clone(), PushReplyLimits::CONTENT).expect("first");
        let second =
            PushReplySlot::from_pool(pool.clone(), PushReplyLimits::CONTENT).expect("second");
        assert!(PushReplySlot::from_pool(pool.clone(), PushReplyLimits::CONTENT).is_err());
        assert_eq!(pool.available_permits(), 0);
        drop(first);
        drop(second);
        assert_eq!(pool.available_permits(), POOL_BYTES);
        let dense = (0..4000)
            .map(|index| (index.to_string(), Value::Null))
            .collect::<serde_json::Map<_, _>>();
        let encoded = serde_json::to_vec(&dense).expect("fixture encoding");
        let shape = preflight_json(
            &encoded,
            JsonStructureLimits {
                max_encoded_bytes: MAX_FRAME_BYTES,
                max_nodes: 65_536,
                max_string_bytes: MAX_FRAME_BYTES,
                max_depth: 64,
            },
        )
        .expect("legal encoded shape");
        assert!(encoded.len() < MAX_FRAME_BYTES);
        assert!(shape.direct_value_decode_bytes().expect("decoded bound") > VALUE_BYTES);
        assert!(
            PushReplySlot::from_pool(pool.clone(), PushReplyLimits::CONTENT)
                .expect("slot")
                .encode(&dense)
                .is_err()
        );
        assert_eq!(pool.available_permits(), POOL_BYTES);
    }
    #[test]
    fn retained_replies_allow_new_construction_after_scratch_release() {
        let pool = Arc::new(Semaphore::new(POOL_BYTES));
        let first = PushReplySlot::from_pool(pool.clone(), PushReplyLimits::CONTENT)
            .expect("first")
            .encode(&"small")
            .expect("reply");
        assert!(pool.available_permits() > POOL_BYTES - 2 * FRAME_OVERHEAD);
        let second = PushReplySlot::from_pool(pool.clone(), PushReplyLimits::CONTENT)
            .expect("independent callback");
        drop(first);
        drop(second);
        assert_eq!(pool.available_permits(), POOL_BYTES);
    }
    struct SmallHandler(Arc<tokio::sync::Barrier>);
    #[async_trait::async_trait]
    impl super::super::PushHandler for SmallHandler {
        fn reply_limits(&self, _: &str) -> Result<PushReplyLimits, PluginRpcError> {
            Ok(PushReplyLimits::ACKNOWLEDGEMENT)
        }
        async fn handle_push(
            &self,
            _: &str,
            _: Value,
            reply: &mut PushReplySlot,
        ) -> Result<PushReply, PluginRpcError> {
            self.0.wait().await;
            reply.encode(&serde_json::json!({"outcome":"applied"}))
        }
    }
    #[tokio::test]
    async fn eight_small_callbacks_construct_concurrently_under_declared_limits() {
        use super::super::PushHandler;
        let pool = Arc::new(Semaphore::new(POOL_BYTES));
        let barrier = Arc::new(tokio::sync::Barrier::new(9));
        let handler = Arc::new(SmallHandler(barrier.clone()));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let handler = handler.clone();
            let mut slot = PushReplySlot::from_pool(
                pool.clone(),
                handler.reply_limits("control").expect("limits"),
            )
            .expect("small callback");
            workers.push(tokio::spawn(async move {
                handler.handle_push("control", Value::Null, &mut slot).await
            }));
        }
        assert!(pool.available_permits() > POOL_BYTES / 2);
        barrier.wait().await;
        for worker in workers {
            drop(worker.await.expect("worker").expect("reply"));
        }
        assert_eq!(pool.available_permits(), POOL_BYTES);
    }
    #[test]
    fn callback_error_keeps_original_slot_through_failure_frame() {
        let pool = Arc::new(Semaphore::new(POOL_BYTES));
        let slot =
            PushReplySlot::from_pool(pool.clone(), PushReplyLimits::ACKNOWLEDGEMENT).expect("slot");
        let error = PluginRpcError {
            code: "failure".into(),
            message: "x".repeat(8192),
        };
        assert_eq!(
            pool.available_permits(),
            POOL_BYTES - PushReplyLimits::ACKNOWLEDGEMENT.construction()
        );
        let frame = slot.failure(RpcId::String("correlation".into()), error);
        assert!(pool.available_permits() < POOL_BYTES);
        drop(frame);
        assert_eq!(pool.available_permits(), POOL_BYTES);
    }
}
