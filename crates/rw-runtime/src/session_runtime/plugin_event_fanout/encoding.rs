//! Bounded source preparation runs outside the actor and Tokio executor.
use super::{error, redaction::Redacted};
use crate::extension_runtime::delivery_budget::MAX_DELIVERY_DECODED_BYTES;
use crate::extension_runtime::{PluginDeliveryBudget, PluginEventSource};
use rw_core::{AgentLoopError, EngineEvent};
use rw_providers::FixtureRedactor;
use rw_tools::CancellationToken;
use rw_types::{
    allocation::AllocationPlan,
    extension_contract::ExtensionDeliveryCursor,
    extension_events::{
        ExtensionEventContent, MAX_EXTENSION_EVENT_INLINE_BYTES, MAX_EXTENSION_EVENT_SOURCE_BYTES,
    },
};
use std::{io::Write, sync::Arc};

pub(super) async fn prepare(
    event: EngineEvent,
    redactor: FixtureRedactor,
    budget: Arc<PluginDeliveryBudget>,
    cancellation: &CancellationToken,
) -> Result<(Arc<PluginEventSource>, ExtensionEventContent), AgentLoopError> {
    let prepared = rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || {
        encode(event, &redactor)
    })
    .await
    .map_err(error)??;
    let retained = budget
        .retain(prepared.charge, cancellation)
        .await
        .map_err(error)?;
    let source =
        PluginEventSource::new(prepared.cursor, prepared.bytes, retained).map_err(error)?;
    Ok((Arc::new(source), prepared.content))
}
struct Encoded {
    cursor: ExtensionDeliveryCursor,
    bytes: Vec<u8>,
    content: ExtensionEventContent,
    charge: usize,
}
fn encode(event: EngineEvent, redactor: &FixtureRedactor) -> Result<Encoded, AgentLoopError> {
    let meta = event
        .meta()
        .ok_or_else(|| error("transient extension event"))?;
    let cursor = ExtensionDeliveryCursor {
        session_id: meta.session_id.clone(),
        sequence: meta.sequence_id,
    };
    let plan =
        AllocationPlan::new(event).map_err(|_| error("extension event allocation overflow"))?;
    if plan.bytes() > MAX_DELIVERY_DECODED_BYTES {
        return Err(error("extension event prepared allocation limit"));
    }
    // The journal owns decoding admission; this allowance covers normalization
    // and encoding. It does not assert that an encoded line bounds serde's heap.
    let event = plan.prepare();
    let mut writer = CappedBytes(Vec::new());
    serde_json::to_writer(
        &mut writer,
        &Redacted {
            value: event.value(),
            redactor,
        },
    )
    .map_err(error)?;
    drop(event);
    let bytes = writer.0;
    let mut charge = bytes
        .capacity()
        .checked_add(128 * 1024)
        .ok_or_else(|| error("extension source charge overflow"))?;
    let mut content = ExtensionEventContent::Source {
        bytes: u32::try_from(bytes.len()).map_err(error)?,
    };
    if bytes.len() <= MAX_EXTENSION_EVENT_INLINE_BYTES {
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(error)?;
        let plan = AllocationPlan::new(value)
            .map_err(|_| error("extension inline allocation overflow"))?;
        if plan.bytes() <= 1024 * 1024 {
            // Keep room for the notice, RPC Value conversion and writer's
            // charged frame while the source remains available to readers.
            charge = charge
                .checked_add(plan.bytes() * 4)
                .ok_or_else(|| error("extension inline charge overflow"))?;
            content = ExtensionEventContent::Inline {
                data: plan.prepare().into_inner(),
            };
        }
    }
    Ok(Encoded {
        cursor,
        bytes,
        content,
        charge,
    })
}
struct CappedBytes(Vec<u8>);
impl Write for CappedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let length = self
            .0
            .len()
            .checked_add(bytes.len())
            .filter(|length| *length <= MAX_EXTENSION_EVENT_SOURCE_BYTES)
            .ok_or_else(|| std::io::Error::other("extension source byte limit"))?;
        if length > self.0.capacity() {
            let capacity = self
                .0
                .capacity()
                .saturating_mul(2)
                .max(4096)
                .max(length)
                .min(MAX_EXTENSION_EVENT_SOURCE_BYTES);
            self.0.reserve_exact(capacity - self.0.len());
        }
        if self.0.capacity() > MAX_EXTENSION_EVENT_SOURCE_BYTES {
            return Err(std::io::Error::other("extension source allocation limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
