//! Correlation state is admitted before a request enters rmcp's queue.
use crate::{McpError, McpInboundRouter, payload_work::Allocation};
use rmcp::model::{ClientRequest, GetExtensions, RequestId};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_REQUESTS: usize = 64;
const REQUEST_METADATA_BYTES: usize = 4096;

pub(in crate::client) struct RequestState {
    method: Box<str>,
    catalog_start: bool,
    reply: Mutex<Option<Arc<Allocation>>>,
    response_claimed: AtomicBool,
    _metadata: Allocation,
    _count: OwnedSemaphorePermit,
    _peer_info: McpInboundRouter,
    _job: crate::payload_work::Job,
}

impl RequestState {
    pub(in crate::client) fn catalog_start(&self) -> bool {
        self.catalog_start
    }

    pub(in crate::client) fn method(&self) -> &str {
        &self.method
    }

    pub(in crate::client) fn install_reply(
        &self,
        retained: Arc<Allocation>,
    ) -> Result<(), McpError> {
        let mut reply = self.reply.lock().map_err(|_| invalid_request())?;
        if reply.is_some() {
            return Err(invalid_request());
        }
        *reply = Some(retained);
        Ok(())
    }

    fn claim_response(&self) -> Result<(), McpError> {
        self.response_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| invalid_request())
    }

    pub(in crate::client) fn reply_retention(&self) -> Result<Arc<Allocation>, McpError> {
        self.reply
            .lock()
            .map_err(|_| invalid_request())?
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(invalid_request)
    }
}

struct RegistryState {
    closed: bool,
    requests: HashMap<RequestId, Weak<RequestState>>,
}

pub(in crate::client) struct RequestRegistry {
    state: Mutex<RegistryState>,
    count: Arc<Semaphore>,
    router: McpInboundRouter,
    jobs: Arc<crate::payload_work::Jobs>,
    _metadata: Allocation,
}

impl RequestRegistry {
    pub(in crate::client) fn new(
        router: McpInboundRouter,
        jobs: Arc<crate::payload_work::Jobs>,
    ) -> Result<Arc<Self>, McpError> {
        let metadata = Allocation::new(MAX_REQUESTS * REQUEST_METADATA_BYTES)?;
        Ok(Arc::new(Self {
            state: Mutex::new(RegistryState {
                closed: false,
                requests: HashMap::with_capacity(MAX_REQUESTS),
            }),
            count: Arc::new(Semaphore::new(MAX_REQUESTS)),
            router,
            jobs,
            _metadata: metadata,
        }))
    }

    pub(in crate::client) fn prepare(
        &self,
        request: &mut ClientRequest,
    ) -> Result<Arc<RequestState>, McpError> {
        // Admission and shutdown share this lock: settlement cannot observe zero
        // jobs before an already-authorized request registers its physical owner.
        let registry = self.state.lock().map_err(|_| invalid_request())?;
        if registry.closed {
            return Err(invalid_request());
        }
        let job = self.jobs.retain()?;
        let method = request.method();
        if method.len() > 256 {
            return Err(invalid_request());
        }
        let count = Arc::clone(&self.count)
            .try_acquire_owned()
            .map_err(|_| invalid_request())?;
        let metadata = Allocation::new(REQUEST_METADATA_BYTES)?;
        let request_state = Arc::new(RequestState {
            method: method.into(),
            catalog_start: matches!(request, ClientRequest::ListToolsRequest(request) if request.params.as_ref().is_none_or(|params| params.cursor.is_none())),
            reply: Mutex::new(None),
            response_claimed: AtomicBool::new(false),
            _metadata: metadata,
            _count: count,
            _peer_info: self.router.clone(),
            _job: job,
        });
        request.extensions_mut().insert(Arc::clone(&request_state));
        drop(registry);
        Ok(request_state)
    }

    pub(in crate::client) fn bind(
        &self,
        id: RequestId,
        request: &ClientRequest,
    ) -> Result<Arc<RequestState>, McpError> {
        let request_state = request
            .extensions()
            .get::<Arc<RequestState>>()
            .cloned()
            .ok_or_else(invalid_request)?;
        let mut state = self.state.lock().map_err(|_| invalid_request())?;
        state
            .requests
            .retain(|_, request| request.strong_count() > 0);
        if state.closed || state.requests.len() >= MAX_REQUESTS || state.requests.contains_key(&id)
        {
            return Err(invalid_request());
        }
        state.requests.insert(id, Arc::downgrade(&request_state));
        Ok(request_state)
    }

    pub(in crate::client) fn claim_response(
        &self,
        route: &super::decode::EnvelopeRoute<'_>,
    ) -> Result<Arc<RequestState>, McpError> {
        let state = self.state.lock().map_err(|_| invalid_request())?;
        if state.closed {
            return Err(invalid_request());
        }
        let id = route.id.ok_or_else(invalid_request)?;
        // Exact string requests take precedence over numeric-string equivalence,
        // as in rmcp. Our generated numeric IDs take the constant-time path.
        let mut matched = None;
        for (key, request) in &state.requests {
            if matches!(key, RequestId::String(_))
                && id.matches_request_id(key).map_err(|_| invalid_request())?
            {
                matched = request.upgrade();
                break;
            }
        }
        let request = matched
            .or_else(|| {
                id.numeric_value()
                    .ok()
                    .flatten()
                    .and_then(|id| state.requests.get(&RequestId::Number(id)))
                    .and_then(Weak::upgrade)
            })
            .ok_or_else(invalid_request)?;
        request.claim_response()?;
        Ok(request)
    }

    pub(in crate::client) fn claim_cancellation(
        &self,
        id: &RequestId,
    ) -> Result<Option<Arc<RequestState>>, McpError> {
        let state = self.state.lock().map_err(|_| invalid_request())?;
        if state.closed {
            return Err(invalid_request());
        }
        // rmcp cancellation uses exact IDs, unlike response numeric-string matching.
        let Some(request) = state.requests.get(id).and_then(Weak::upgrade) else {
            return Ok(None);
        };
        if request.claim_response().is_err() {
            return Ok(None);
        }
        Ok(Some(request))
    }

    pub(in crate::client) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.requests.clear();
        }
    }
}

fn invalid_request() -> McpError {
    McpError::Protocol("MCP request correlation admission failed".into())
}
