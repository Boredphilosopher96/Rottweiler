//! Completion survives unwinding in both the operation and its acknowledgement path.
use super::{
    Arc, ClientId, DedupeState, EngineHost, RequestId, RetainedDispatch, trim_dedupe, watch,
};

pub(super) struct CompletionGuard {
    host: EngineHost,
    key: (ClientId, RequestId),
    payload_hash: String,
    fallback: Arc<RetainedDispatch>,
    completion: Option<watch::Sender<Option<Arc<RetainedDispatch>>>>,
}

impl CompletionGuard {
    pub(super) fn new(
        host: &EngineHost,
        key: &(ClientId, RequestId),
        payload_hash: &str,
        fallback: Arc<RetainedDispatch>,
    ) -> Self {
        let completion = match host
            .dedupe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .get(key)
        {
            Some(DedupeState::Running { completion, .. }) => Some(completion.clone()),
            _ => None,
        };
        Self {
            host: host.clone(),
            key: key.clone(),
            payload_hash: payload_hash.into(),
            fallback,
            completion,
        }
    }

    pub(super) fn complete(&mut self) {
        self.completion = None;
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        self.host.control_owner.fail();
        self.host
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        // No injected clock, serialization, async cleanup, or extension callback
        // may be needed to release waiters after an unwind.
        let dispatch = Arc::clone(&self.fallback);
        let mut ledger = self
            .host
            .dedupe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger.entries.insert(
            self.key.clone(),
            DedupeState::Complete {
                payload_hash: self.payload_hash.clone(),
                dispatch: dispatch.clone(),
                retry_same_request: false,
            },
        );
        trim_dedupe(&mut ledger, self.host.config.max_deduplicated_requests);
        drop(ledger);
        completion.send_replace(Some(dispatch));
    }
}
