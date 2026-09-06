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
use std::sync::{Arc, Mutex, OnceLock};
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
    /// One host tool outcome, including its bounded result and correlation fields.
    pub const TOOL_OUTCOME: Self = Self {
        encoded: rw_types::extension_tools::MAX_EXTENSION_TOOL_OUTPUT_BYTES + 16 * 1024,
        decoded: VALUE_BYTES,
    };
    /// One plugin namespace, including entry framing and source cursors.
    pub const STATE: Self = Self {
        encoded: rw_types::extension_contract::MAX_EXTENSION_NAMESPACE_BYTES + 16 * 1024,
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
    credit: Arc<Mutex<OwnedSemaphorePermit>>,
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
            credit: Arc::new(Mutex::new(
                pool.try_acquire_many_owned(count)
                    .map_err(|_| unavailable())?,
            )),
        })
    }
    /// Serialize and structurally admit a typed response in an owned CPU worker.
    /// # Errors
    /// Rejects physical admission, oversized encodings or decoded amplification.
    pub async fn encode<T: Serialize + Send + 'static>(
        &mut self,
        source: T,
    ) -> Result<PushReply, PluginRpcError> {
        self.encode_source(source, None).await
    }
    /// Move the upstream source allowance into the physical transformation worker.
    /// # Errors
    /// Rejects physical admission, oversized encodings or decoded amplification.
    pub async fn encode_retained<T: Serialize + Send + 'static>(
        &mut self,
        source: T,
        owner: impl Send + Sync + 'static,
    ) -> Result<PushReply, PluginRpcError> {
        self.encode_source(source, Some(Box::new(owner))).await
    }
    async fn encode_source<T: Serialize + Send + 'static>(
        &mut self,
        source: T,
        owner: Option<Box<dyn Send + Sync>>,
    ) -> Result<PushReply, PluginRpcError> {
        let work = EncodingWork {
            source,
            owner,
            slot: Self {
                credit: Arc::clone(&self.credit),
                limits: self.limits,
            },
        };
        let (result, _slot) =
            rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
                .await
                .map_err(|error| rpc_error("reply_worker", &error.to_string()))?;
        result
    }
    fn encode_value(&self, source: &impl Serialize) -> Result<PushReply, PluginRpcError> {
        if self
            .credit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .num_permits()
            < self.limits.construction()
        {
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
            credit: self
                .credit
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .split(bytes)
                .ok_or_else(unavailable)?,
            source: None,
        };
        Ok(PushReply {
            value: value.into_inner(),
            retained,
            limits: self.limits,
        })
    }
    pub(super) fn failure(
        self,
        id: RpcId,
        error: PluginRpcError,
    ) -> Result<OwnedPushFrame, PluginRpcError> {
        // A cancelled encode waiter can leave a physical worker using this slot.
        // Its construction credit cannot be transferred to an unrelated error frame.
        if Arc::strong_count(&self.credit) != 1 {
            return Err(unavailable());
        }
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
        let mut credit = self
            .credit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = credit.num_permits();
        let retained = credit.split(count).ok_or_else(unavailable)?;
        Ok(OwnedPushFrame {
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
                credit: retained,
                source: None,
            },
        })
    }
}

// Drop typed source allocations before their upstream and construction allowances,
// including cancellation while the CPU admission future still owns this work.
struct EncodingWork<T> {
    source: T,
    owner: Option<Box<dyn Send + Sync>>,
    slot: PushReplySlot,
}
impl<T: Serialize> EncodingWork<T> {
    fn run(self) -> (Result<PushReply, PluginRpcError>, PushReplySlot) {
        let result = self.slot.encode_value(&self.source);
        drop(self.source);
        let result = match result {
            Ok(mut reply) => {
                reply.retained.source = self.owner;
                Ok(reply)
            }
            Err(error) => {
                drop(self.owner);
                Err(error)
            }
        };
        (result, self.slot)
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
    pub(super) async fn redact(
        self,
        id: RpcId,
        redactor: Arc<dyn PluginBoundaryRedactor>,
    ) -> Result<OwnedPushFrame, PluginRpcError> {
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || {
            self.redact_value_owned(id, redactor.as_ref())
        })
        .await
        .map_err(|error| rpc_error("reply_worker", &error.to_string()))?
    }
    fn redact_value_owned(
        mut self,
        id: RpcId,
        redactor: &dyn PluginBoundaryRedactor,
    ) -> Result<OwnedPushFrame, PluginRpcError> {
        let mut bytes = self.value.prepared_bytes().ok_or_else(unavailable)?;
        redact_value(
            &mut self.value,
            redactor,
            &mut bytes,
            self.limits.decoded,
            &mut self.retained,
        )?;
        // The admitted Value has normalized maps/arrays; replacing string leaves
        // preserves that ownership. No second map reconstruction is necessary.
        let id_bytes = match &id {
            RpcId::String(value) => value.capacity(),
            RpcId::Number(_) => 0,
        };
        let bytes = bytes
            .checked_add(FRAME_OVERHEAD)
            .and_then(|bytes| bytes.checked_add(id_bytes))
            .ok_or_else(unavailable)?;
        self.retained.ensure(bytes)?;
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
    retained: &mut ReplyRetention,
) -> Result<(), PluginRpcError> {
    match value {
        Value::String(text) => {
            let other = bytes.checked_sub(text.capacity()).ok_or_else(unavailable)?;
            let limit = ceiling.checked_sub(other).ok_or_else(unavailable)?;
            let current = *bytes;
            let replacement = redactor.redact_reply_text(text, limit, &mut |scratch| {
                let required = current
                    .checked_add(scratch)
                    .and_then(|value| value.checked_add(FRAME_OVERHEAD))
                    .ok_or_else(|| std::io::Error::other("reply admission overflow"))?;
                retained
                    .ensure(required)
                    .map_err(|_| std::io::Error::other("reply working admission exhausted"))
            })?;
            *bytes = other
                .checked_add(replacement.capacity())
                .filter(|bytes| *bytes <= ceiling)
                .ok_or_else(unavailable)?;
            *text = replacement;
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value, redactor, bytes, ceiling, retained)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_value(value, redactor, bytes, ceiling, retained)?;
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
    fn ensure(&mut self, bytes: usize) -> Result<(), PluginRpcError> {
        if bytes > self.credit.num_permits() {
            let additional =
                u32::try_from(bytes - self.credit.num_permits()).map_err(|_| unavailable())?;
            let credit = Arc::clone(self.credit.semaphore())
                .try_acquire_many_owned(additional)
                .map_err(|_| unavailable())?;
            self.credit.merge(credit);
        }
        Ok(())
    }
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
    #[tokio::test]
    async fn construction_admission_precedes_callback_and_refunds_after_rejection() {
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
                .encode(dense)
                .await
                .is_err()
        );
        assert_eq!(pool.available_permits(), POOL_BYTES);
    }
    #[tokio::test]
    async fn retained_replies_allow_new_construction_after_scratch_release() {
        let pool = Arc::new(Semaphore::new(POOL_BYTES));
        let first = PushReplySlot::from_pool(pool.clone(), PushReplyLimits::CONTENT)
            .expect("first")
            .encode("small")
            .await
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
            reply.encode(serde_json::json!({"outcome":"applied"})).await
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
    struct BlockedSource {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Serialize for BlockedSource {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if let Some(entered) = self.entered.lock().expect("entry lock").take() {
                let _ = entered.send(());
            }
            self.release
                .lock()
                .expect("release lock")
                .recv()
                .expect("release worker");
            serializer.serialize_str("settled")
        }
    }
    #[tokio::test]
    async fn cancelled_encode_keeps_source_and_construction_credit_until_worker_settles() {
        let pool = Arc::new(Semaphore::new(POOL_BYTES));
        let mut slot =
            PushReplySlot::from_pool(pool.clone(), PushReplyLimits::ACKNOWLEDGEMENT).expect("slot");
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, receiver) = std::sync::mpsc::channel();
        let source = BlockedSource {
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(receiver),
        };
        let mut encode = Box::pin(slot.encode_retained(source, owner));
        tokio::select! {
            result = &mut encode => panic!("worker unexpectedly completed: {}", result.is_ok()),
            result = entry => result.expect("worker entered"),
        }
        // The single-threaded executor remains available while Serialize is blocked.
        tokio::task::yield_now().await;
        drop(encode);
        assert!(weak.upgrade().is_some());
        assert!(slot.failure(RpcId::Number(1), unavailable()).is_err());
        assert_eq!(
            pool.available_permits(),
            POOL_BYTES - PushReplyLimits::ACKNOWLEDGEMENT.construction()
        );
        release.send(()).expect("release");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while weak.upgrade().is_some() || pool.available_permits() != POOL_BYTES {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("physical worker settled");
    }

    #[tokio::test]
    async fn nested_state_redaction_progresses_beside_two_tool_callback_owners() {
        use super::super::NoopPluginBoundaryRedactor;
        let pool = Arc::new(Semaphore::new(POOL_BYTES));
        let outer = PushReplySlot::from_pool(pool.clone(), PushReplyLimits::TOOL_OUTCOME)
            .expect("outer tool effect");
        let mut concurrent = PushReplySlot::from_pool(pool.clone(), PushReplyLimits::TOOL_OUTCOME)
            .expect("concurrent tool effect");
        let mut inner = PushReplySlot::from_pool(pool.clone(), PushReplyLimits::STATE)
            .expect("nested state read");
        assert!(PushReplySlot::from_pool(pool.clone(), PushReplyLimits::CONTENT).is_err());
        let result = inner
            .encode(serde_json::json!({"entries":[{"key":"task","value":"ready"}]}))
            .await
            .expect("state encoding");
        drop(inner);
        let state = result
            .redact(RpcId::Number(2), Arc::new(NoopPluginBoundaryRedactor))
            .await
            .expect("nested state redaction");
        assert!(
            matches!(&state.frame, RpcFrame::Success(value) if value.result["entries"][0]["value"] == "ready")
        );
        let body = "x".repeat(rw_types::extension_tools::MAX_EXTENSION_TOOL_OUTPUT_BYTES - 2);
        let result = concurrent.encode(body).await.expect("large tool result");
        drop(concurrent);
        let result = result
            .redact(RpcId::Number(3), Arc::new(NoopPluginBoundaryRedactor))
            .await
            .expect("large result redaction");
        assert!(
            matches!(&result.frame, RpcFrame::Success(value) if value.result.as_str().map(str::len) == Some(rw_types::extension_tools::MAX_EXTENSION_TOOL_OUTPUT_BYTES - 2))
        );
        drop(result);
        drop(state);
        drop(outer);
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
        let frame = slot
            .failure(RpcId::String("correlation".into()), error)
            .expect("failure frame");
        assert!(pool.available_permits() < POOL_BYTES);
        drop(frame);
        assert_eq!(pool.available_permits(), POOL_BYTES);
    }
}
