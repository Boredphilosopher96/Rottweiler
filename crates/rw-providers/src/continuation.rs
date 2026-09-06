//! Keeps adapter-owned reasoning payloads within their producing route.

use rw_types::Block;
use serde::{Deserialize, Serialize};

use crate::{ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest};

const MAX_CONTINUATION_BYTES: usize = 256 * 1024;

/// Digest of the adapter configuration and authority that produced opaque state.
/// It contains no endpoint, credential reference, or plugin path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContinuationProvenance([u8; 32]);

impl ContinuationProvenance {
    /// Binds an ordered sequence of configuration identities without ambiguity.
    #[must_use]
    pub fn bind(parts: &[&[u8]]) -> Self {
        let mut hash =
            blake3::Hasher::new_derive_key("Rottweiler provider continuation provenance");
        for part in parts {
            hash.update(&(part.len() as u64).to_le_bytes());
            hash.update(part);
        }
        Self(*hash.finalize().as_bytes())
    }

    /// Extends adapter provenance with its host configuration identity.
    #[must_use]
    pub fn qualified(&self, identity: &[u8]) -> Self {
        Self::bind(&[&self.0, identity])
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    scope: ContinuationProvenance,
    payload: String,
}

pub(crate) struct ContinuationScope(Option<ContinuationProvenance>);

impl ContinuationScope {
    pub(crate) fn new(
        provider: &str,
        model: &str,
        provenance: Option<ContinuationProvenance>,
    ) -> Self {
        Self(provenance.map(|provenance| {
            ContinuationProvenance::bind(&[&provenance.0, provider.as_bytes(), model.as_bytes()])
        }))
    }

    pub(crate) fn open(&self, request: &mut ProviderRequest) -> Result<(), ProviderError> {
        for block in request.turns.iter_mut().flat_map(|turn| &mut turn.blocks) {
            if let Block::Thinking {
                signature: Some(signature),
                ..
            } = block
            {
                if signature.len() > MAX_CONTINUATION_BYTES {
                    return Err(incompatible());
                }
                let envelope: Envelope =
                    serde_json::from_str(signature).map_err(|_| incompatible())?;
                if self.0.as_ref() != Some(&envelope.scope) {
                    return Err(incompatible());
                }
                *signature = envelope.payload;
            }
        }
        Ok(())
    }

    pub(crate) fn seal(&self, event: &mut ProviderEvent) -> Result<(), ProviderError> {
        if matches!(event, ProviderEvent::RouteSelected { .. }) {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "providers cannot emit router control events",
            ));
        }
        if let ProviderEvent::ThinkingDelta {
            signature: Some(signature),
            ..
        } = event
        {
            let scope = self.0.as_ref().ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "provider emitted continuation state without declared provenance",
                )
            })?;
            if signature.len() > MAX_CONTINUATION_BYTES {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "provider continuation exceeds its byte limit",
                ));
            }
            let encoded = serde_json::to_string(&Envelope {
                scope: scope.clone(),
                payload: std::mem::take(signature),
            })
            .map_err(|_| incompatible())?;
            if encoded.len() > MAX_CONTINUATION_BYTES {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "provider continuation exceeds its byte limit",
                ));
            }
            *signature = encoded;
        }
        Ok(())
    }
}

fn incompatible() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        "conversation continuation does not belong to this provider, model, and configuration",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rw_types::{Role, Turn, TurnMeta};

    fn scoped(provider: &str, model: &str, configuration: &[u8]) -> ContinuationScope {
        ContinuationScope::new(
            provider,
            model,
            Some(ContinuationProvenance::bind(&[configuration])),
        )
    }

    fn request(signature: String) -> ProviderRequest {
        ProviderRequest {
            model: "model".to_owned(),
            turns: vec![Turn {
                role: Role::Assistant,
                blocks: vec![Block::Thinking {
                    content: "reason".to_owned(),
                    signature: Some(signature),
                }],
                meta: TurnMeta::default(),
            }],
            tools: vec![],
            tool_choice: crate::ToolChoice::Auto {},
            max_output_tokens: 10,
            temperature: None,
            thinking: rw_types::config::ThinkingLevel::Off,
            cache_hint: None,
        }
    }

    #[test]
    fn opaque_state_is_bound_to_provider_model_and_provenance() -> Result<(), ProviderError> {
        let mut event = ProviderEvent::ThinkingDelta {
            content: String::new(),
            signature: Some("adapter-owned".to_owned()),
        };
        scoped("provider", "model", b"configuration").seal(&mut event)?;
        let ProviderEvent::ThinkingDelta {
            signature: Some(signature),
            ..
        } = event
        else {
            panic!("signature must be preserved")
        };
        for scope in [
            scoped("other", "model", b"configuration"),
            scoped("provider", "other", b"configuration"),
            scoped("provider", "model", b"other"),
            ContinuationScope::new("provider", "model", None),
        ] {
            assert!(scope.open(&mut request(signature.clone())).is_err());
        }
        let mut restored = request(signature);
        scoped("provider", "model", b"configuration").open(&mut restored)?;
        assert_eq!(restored, request("adapter-owned".to_owned()));
        Ok(())
    }

    #[test]
    fn malformed_or_unscoped_state_is_rejected() {
        let scope = scoped("provider", "model", b"configuration");
        for signature in ["adapter-owned", "{}", "{\"scope\":[],\"payload\":\"x\"}"] {
            assert!(scope.open(&mut request(signature.to_owned())).is_err());
        }
        let mut event = ProviderEvent::ThinkingDelta {
            content: String::new(),
            signature: Some("opaque".to_owned()),
        };
        assert!(
            ContinuationScope::new("provider", "model", None)
                .seal(&mut event)
                .is_err()
        );
    }

    #[test]
    fn continuation_bounds_include_encoded_envelope_bytes() {
        let scope = scoped("provider", "model", b"configuration");
        let mut event = ProviderEvent::ThinkingDelta {
            content: String::new(),
            signature: Some("x".repeat(MAX_CONTINUATION_BYTES)),
        };
        assert!(scope.seal(&mut event).is_err());
        assert!(
            scope
                .open(&mut request("x".repeat(MAX_CONTINUATION_BYTES + 1)))
                .is_err()
        );
    }
}
