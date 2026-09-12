//! Native-only payload attachments; serialized extension results cannot mint these references.
use crate::ToolError;
use rw_types::{SessionPayloadReference, session_payload::MAX_TOOL_PAYLOADS};
use std::sync::Arc;

/// Host-retained payload metadata and allocation/namespace owners through canonical publication.
/// This carrier is absent from the public tool response wire schema.
#[derive(Clone, Default)]
pub struct ToolResultPayloads(Option<Arc<Vec<Attachment>>>);
#[derive(Clone)]
struct Attachment {
    reference: Option<SessionPayloadReference>,
    _retained: Arc<dyn Send + Sync>,
}
impl ToolResultPayloads {
    /// Retains one native result owner even when no durable body attachment is needed.
    /// # Errors
    /// Rejects malformed references and more than the shared attachment limit.
    pub fn retained(
        references: Vec<SessionPayloadReference>,
        retained: Arc<dyn Send + Sync>,
    ) -> Result<Self, ToolError> {
        if references.len() > MAX_TOOL_PAYLOADS {
            return Err(ToolError::Output(
                "tool payload reference limit exceeded".into(),
            ));
        }
        let mut carrier = Self::default();
        for reference in references {
            carrier.attach(reference, Arc::clone(&retained))?;
        }
        if carrier.0.is_none() {
            carrier.0 = Some(Arc::new(vec![Attachment {
                reference: None,
                _retained: retained,
            }]));
        }
        Ok(carrier)
    }

    pub(crate) fn attach(
        &mut self,
        reference: SessionPayloadReference,
        retained: Arc<dyn Send + Sync>,
    ) -> Result<(), ToolError> {
        reference
            .validate()
            .map_err(|message| ToolError::Output(message.to_owned()))?;
        if self
            .0
            .iter()
            .flat_map(|attachments| attachments.iter())
            .filter(|attachment| attachment.reference.is_some())
            .count()
            == MAX_TOOL_PAYLOADS
        {
            return Err(ToolError::Output(
                "tool payload reference limit exceeded".to_owned(),
            ));
        }
        Arc::make_mut(self.0.get_or_insert_with(|| Arc::new(Vec::new()))).push(Attachment {
            reference: Some(reference),
            _retained: retained,
        });
        Ok(())
    }

    /// Produces only the bounded metadata for the durable completion event.
    /// The original carrier retains all physical/result owners until publication settles.
    #[must_use]
    pub fn references(&self) -> Vec<SessionPayloadReference> {
        self.0
            .iter()
            .flat_map(|attachments| attachments.iter())
            .filter_map(|attachment| attachment.reference.clone())
            .collect()
    }
}
impl std::fmt::Debug for ToolResultPayloads {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_list()
            .entries(
                self.0
                    .iter()
                    .flat_map(|attachments| attachments.iter())
                    .filter_map(|attachment| attachment.reference.as_ref()),
            )
            .finish()
    }
}
impl Eq for ToolResultPayloads {}
impl PartialEq for ToolResultPayloads {
    fn eq(&self, other: &Self) -> bool {
        self.0
            .iter()
            .flat_map(|attachments| attachments.iter())
            .filter_map(|attachment| attachment.reference.as_ref())
            .eq(other
                .0
                .iter()
                .flat_map(|attachments| attachments.iter())
                .filter_map(|attachment| attachment.reference.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Retained(Arc<AtomicBool>);
    impl Drop for Retained {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    #[test]
    fn compact_result_keeps_allocation_without_minting_attachment() -> Result<(), ToolError> {
        let dropped = Arc::new(AtomicBool::new(false));
        let carrier =
            ToolResultPayloads::retained(Vec::new(), Arc::new(Retained(dropped.clone())))?;
        assert_eq!(carrier, ToolResultPayloads::default());
        assert!(carrier.references().is_empty());
        assert!(!dropped.load(Ordering::Acquire));
        drop(carrier);
        assert!(dropped.load(Ordering::Acquire));
        Ok(())
    }
    #[test]
    fn result_attachment_is_not_forgeable_through_extension_response_json()
    -> Result<(), Box<dyn std::error::Error>> {
        let forged = serde_json::json!({"content":"x","data":null,"truncated":false,"payloads":[{"digest":"0".repeat(64),"bytes":1}]});
        assert!(serde_json::from_value::<crate::ToolResult>(forged).is_err());
        let dropped = Arc::new(AtomicBool::new(false));
        let reference = SessionPayloadReference {
            digest: "0".repeat(64),
            bytes: 1,
        };
        let mut result = crate::ToolResult::new("x", serde_json::Value::Null)
            .with_payload(reference.clone(), Arc::new(Retained(Arc::clone(&dropped))))?;
        let encoded = serde_json::to_value(&result)?;
        assert!(encoded.get("payloads").is_none());
        let carrier = result.take_payloads();
        drop(result);
        assert!(!dropped.load(Ordering::Acquire));
        let copy = carrier.clone();
        drop(carrier);
        assert_eq!(copy.references(), vec![reference]);
        assert!(!dropped.load(Ordering::Acquire));
        drop(copy);
        assert!(dropped.load(Ordering::Acquire));
        Ok(())
    }
}
