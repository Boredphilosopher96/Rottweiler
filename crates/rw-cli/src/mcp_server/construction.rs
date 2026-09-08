//! CLI reply construction uses the server's admitted slot through every CPU kernel.
use super::{BridgeError, CliMcpBridge, EngineTool, McpResponse, McpResponseSlot, SessionSummary};
use rw_core::{ClientCommand, EngineEvent, HostEvent, RequestId};
use rw_tools::ToolRegistry;
use rw_types::{
    allocation::{AllocationPlan, PrepareAllocation},
    json_encoding::JsonWriter,
    json_structure::{JsonStructureLimits, preflight_json},
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

// Four fixed readonly producers cap their encoded result at 256 KiB. This slot
// also covers Read's line-index capacity, permission argument copies, normalized
// maps and result conversion. Canonical control decoding has its own checked
// structural profile inside the same slot; encoded HostEvent bytes retain theirs.
pub(super) const WORKING_BYTES: usize = 16 * 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 256 * 1024;
const MAX_CONTROL_BYTES: usize = 4 * 1024 * 1024;
const HOST_RESULT_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Construction<T> {
    pub(super) value: T,
    pub(super) slot: McpResponseSlot,
}
impl<T> Construction<T> {
    pub(super) const fn new(value: T, slot: McpResponseSlot) -> Self {
        Self { value, slot }
    }
}
impl<T: Send + 'static> Construction<T> {
    pub(super) async fn map_cpu<U: Send + 'static>(
        self,
        transform: impl FnOnce(T) -> Result<U, BridgeError> + Send + 'static,
    ) -> Result<Construction<U>, BridgeError> {
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || {
            // Capture the structural owner whole even before the worker's first poll.
            self.map(transform)
        })
        .await
        .map_err(|_| unavailable())?
    }
    fn map<U>(
        self,
        transform: impl FnOnce(T) -> Result<U, BridgeError>,
    ) -> Result<Construction<U>, BridgeError> {
        Ok(Construction {
            value: transform(self.value)?,
            slot: self.slot,
        })
    }
}
impl<T: PrepareAllocation + Send + 'static> Construction<T> {
    pub(super) async fn adopt(self) -> Result<McpResponse<T>, BridgeError> {
        self.slot.adopt(self.value).await.map_err(|_| unavailable())
    }
}

pub(super) fn tool_descriptors(
    registry: Arc<ToolRegistry>,
) -> Result<Vec<EngineTool>, BridgeError> {
    let mut bytes = 4096usize;
    let mut count = 0;
    for descriptor in registry.descriptor_refs() {
        count += 1;
        bytes = bytes
            .checked_add(descriptor.name.capacity())
            .and_then(|bytes| bytes.checked_add(descriptor.description.capacity()))
            .and_then(|bytes| bytes.checked_add(descriptor.input_schema.prepared_bytes()?))
            .ok_or_else(unavailable)?;
    }
    if count > 4 || bytes > WORKING_BYTES / 3 {
        return Err(unavailable());
    }
    let result = registry
        .descriptor_refs()
        .map(|descriptor| EngineTool {
            name: descriptor.name.clone(),
            description: descriptor.description.clone(),
            input_schema: descriptor.input_schema.clone(),
        })
        .collect();
    drop(registry);
    Ok(result)
}

pub(super) fn tool_arguments(arguments: Value) -> Result<(Value, Value), BridgeError> {
    JsonWriter::count(MAX_ARGUMENT_BYTES)
        .serialize(&arguments)
        .map_err(|_| unavailable())?;
    let plan = AllocationPlan::new(arguments).map_err(|_| unavailable())?;
    if plan.bytes() > WORKING_BYTES / 4 {
        return Err(unavailable());
    }
    let arguments = plan.prepare().into_inner();
    let permission = arguments.clone();
    Ok((arguments, permission))
}

pub(super) fn validate_authorized(ids: &mut [String]) -> Result<(), BridgeError> {
    if ids.len() > rw_mcp::MAX_SERVER_SESSIONS
        || ids
            .iter()
            .any(|id| rw_core::SessionId::validate(id).is_err())
    {
        return Err(BridgeError::safe("session authorization is invalid"));
    }
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(BridgeError::safe("session authorization is invalid"));
    }
    Ok(())
}

pub(super) async fn create_session(
    bridge: &CliMcpBridge,
    mut slot: McpResponseSlot,
) -> Result<McpResponse<SessionSummary>, BridgeError> {
    let meta = bridge.next_meta();
    let request = meta.request_id.clone();
    let mut events = bridge
        .host
        .subscribe(bridge.bound.clone(), None, None)
        .await
        .map_err(|_| unavailable())?;
    let response = bridge
        .host
        .dispatch(
            bridge.bound.clone(),
            ClientCommand::CreateSession {
                meta,
                cwd: bridge.workspace.clone(),
                model: None,
            },
        )
        .await;
    super::require_accepted(&response.outcome, "engine session request was rejected")?;
    // CreateSession is a control command: its exact correlated singleton arrives
    // on the subscribed host stream, never through a catalog/read reply.
    tokio::time::timeout(HOST_RESULT_TIMEOUT, async {
        while let Some(event) = events.recv().await {
            let request = request.clone();
            let work = Construction::new(event.map_err(|_| unavailable())?, slot)
                .map_cpu(move |event| created_session(event, &request))
                .await?;
            if let Some(session) = work.value {
                return Construction::new(session, work.slot).adopt().await;
            }
            slot = work.slot;
        }
        Err(unavailable())
    })
    .await
    .map_err(|_| BridgeError::safe("engine session request timed out"))?
}

fn created_session(
    event: HostEvent,
    request: &RequestId,
) -> Result<Option<SessionSummary>, BridgeError> {
    // Preflight itself can use twice the encoded size for escaped-string scratch.
    // The frame's encoded allocation is independently retained by HostEvent.
    if event.json.len() > MAX_CONTROL_BYTES {
        return Err(unavailable());
    }
    let shape = preflight_json(
        &event.json,
        JsonStructureLimits {
            max_encoded_bytes: MAX_CONTROL_BYTES,
            max_nodes: 65_536,
            max_string_bytes: MAX_CONTROL_BYTES,
            max_depth: 64,
        },
    )
    .map_err(|_| unavailable())?;
    let bytes = shape
        .decode_bytes::<EngineEvent>()
        .and_then(|bytes| bytes.checked_add(event.json.len() * 2));
    if bytes.is_none_or(|bytes| bytes > WORKING_BYTES - 4096) {
        return Err(unavailable());
    }
    let decoded: EngineEvent = serde_json::from_slice(&event.json).map_err(|_| unavailable())?;
    if let EngineEvent::SessionsListed { meta, mut sessions } = decoded {
        if meta.request_id == *request {
            if sessions.len() != 1 {
                return Err(unavailable());
            }
            let session = sessions.pop().ok_or_else(unavailable)?;
            return Ok(Some(SessionSummary {
                id: session.session_id.0,
                state: "driver".to_owned(),
            }));
        }
    }
    Ok(None)
}
fn unavailable() -> BridgeError {
    BridgeError::safe("engine response construction is unavailable")
}

#[cfg(test)]
mod tests;
