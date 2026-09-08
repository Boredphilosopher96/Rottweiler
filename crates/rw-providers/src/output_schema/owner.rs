//! One validation owner from adapter admission through physical CPU settlement.
use super::{MAX_STRUCTURED_OUTPUT_BYTES, OutputContract, OutputSchema};
use crate::{
    BoxEventStream, FinishReason, ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest,
    ToolChoice,
};
use futures_util::StreamExt;
use std::sync::{Arc, LazyLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// Eight simultaneous structured responses. Each claim covers 256 KiB accumulated
// UTF-8, 512 KiB serde escaped-string scratch, 64 KiB schema, a bounded 256-node
// wire-schema graph (<512 KiB), and temporary publication/counter overlap. Text
// requests never access this owner. Provider HTTP/frame owners remain separate.
const WORK_BYTES: u32 = 2 * 1024 * 1024;
const POOL_BYTES: usize = 16 * 1024 * 1024;
static WORKING: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(POOL_BYTES)));

/// Installs the single final-output validator before a provider begins effects.
/// Keep this owner alive through request encoding and transfer it into `attach`.
pub struct OutputValidation {
    work: Option<ValidationWork>,
    fingerprint: Option<[u8; 32]>,
}
struct ValidationWork {
    schema: OutputSchema,
    text: String,
    _permit: OwnedSemaphorePermit,
}
impl ValidationWork {
    fn validate(self) -> Result<Self, ProviderError> {
        super::validate::validate(&self.schema, &self.text)
            .map_err(|_| invalid_response("structured output failed its JSON schema"))?;
        Ok(self)
    }
}
impl OutputValidation {
    /// Reject unsupported or incompatible contracts before metadata/auth discovery.
    /// This performs no allocation or capacity reservation on the text path.
    /// # Errors
    /// Rejects malformed schemas, structured tools, and unsupported adapters.
    pub fn preflight(request: &ProviderRequest, supported: bool) -> Result<(), ProviderError> {
        request.output.validate()?;
        if matches!(request.output, OutputContract::Text {}) {
            return Ok(());
        }
        if !request.tools.is_empty() || !matches!(request.tool_choice, ToolChoice::None {}) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "structured output requires no tools and tool_choice none",
            ));
        }
        if !supported {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "selected adapter does not support structured output",
            ));
        }
        Ok(())
    }

    /// Acquire all decoding/encoding working capacity before dispatch.
    /// # Errors
    /// Rejects invalid input, unsupported adapters, and aggregate pressure.
    pub fn prepare(request: &ProviderRequest, supported: bool) -> Result<Self, ProviderError> {
        Self::preflight(request, supported)?;
        let OutputContract::JsonSchema { schema, .. } = &request.output else {
            return Ok(Self {
                work: None,
                fingerprint: None,
            });
        };
        Self::admit(&request.output, schema, &WORKING)
    }

    fn admit(
        contract: &OutputContract,
        schema: &OutputSchema,
        pool: &Arc<Semaphore>,
    ) -> Result<Self, ProviderError> {
        let permit = pool
            .clone()
            .try_acquire_many_owned(WORK_BYTES)
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::ResourceExhausted,
                    "structured output working capacity is saturated",
                )
            })?;
        let fingerprint = fingerprint(contract)?;
        Ok(Self {
            work: Some(ValidationWork {
                schema: schema.clone(),
                text: String::with_capacity(MAX_STRUCTURED_OUTPUT_BYTES),
                _permit: permit,
            }),
            fingerprint,
        })
    }

    /// Withhold structured text until the complete final response is valid.
    /// Truncation, refusal, tool calls, cancellation and invalid JSON never emit
    /// a successful terminal. The actual CPU worker owns all bodies and credit.
    #[must_use]
    pub fn attach(self, mut source: BoxEventStream) -> BoxEventStream {
        let Some(mut work) = self.work else {
            return source;
        };
        let stream = async_stream::try_stream! {
            let mut finished = false;
            let mut events = 0_usize;
            while let Some(event) = source.next().await {
                events += 1;
                if events > 4_096 { Err(invalid_response("structured output exceeds its event limit"))?; }
                let event = event?;
                if finished { Err(invalid_response("structured output continued after its terminal"))?; }
                match event {
                    ProviderEvent::TextDelta { text } => {
                        if text.len() > MAX_STRUCTURED_OUTPUT_BYTES.saturating_sub(work.text.len()) {
                            Err(invalid_response("structured output exceeds its byte limit"))?;
                        }
                        work.text.push_str(&text);
                    }
                    ProviderEvent::Finished { reason: FinishReason::Stop } => finished = true,
                    ProviderEvent::Finished { reason: FinishReason::ContentFilter } => Err(invalid_response("structured output was refused"))?,
                    ProviderEvent::Finished { reason: FinishReason::Length } => Err(invalid_response("structured output was truncated"))?,
                    ProviderEvent::Finished { .. } | ProviderEvent::ToolCallStart { .. } | ProviderEvent::ToolCallArgumentsDelta { .. } | ProviderEvent::ToolCallEnd { .. } => Err(invalid_response("structured output did not complete normally"))?,
                    event => yield event,
                }
            }
            if !finished { Err(invalid_response("structured output ended without completion"))?; }
            drop(source);
            let work = rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.validate()).await
                .map_err(|error| worker_error(&error))??;
            let mut offset = 0;
            while offset < work.text.len() {
                let mut end = work.text.len().min(offset + 16 * 1024);
                while !work.text.is_char_boundary(end) { end -= 1; }
                yield ProviderEvent::TextDelta { text: work.text[offset..end].to_owned() };
                offset = end;
            }
            drop(work);
            yield ProviderEvent::Finished { reason: FinishReason::Stop };
        };
        let mut result = BoxEventStream::new(stream);
        result.output_contract = self.fingerprint;
        result
    }

    pub(crate) fn require_installed(
        stream: &BoxEventStream,
        expected: Option<[u8; 32]>,
    ) -> Result<(), ProviderError> {
        if stream.output_contract != expected {
            return Err(invalid_response(
                "provider omitted the required output validator",
            ));
        }
        Ok(())
    }
}
pub(crate) fn fingerprint(contract: &OutputContract) -> Result<Option<[u8; 32]>, ProviderError> {
    if matches!(contract, OutputContract::Text {}) {
        return Ok(None);
    }
    let mut hasher = blake3::Hasher::new();
    rw_types::json_encoding::JsonWriter::stream(&mut hasher, 256 * 1024)
        .serialize(contract)
        .map_err(|_| invalid_response("structured output contract could not be identified"))?;
    Ok(Some(*hasher.finalize().as_bytes()))
}
fn invalid_response(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Protocol, message)
}

fn worker_error(error: &rw_resources::WorkError) -> ProviderError {
    let kind = match error {
        rw_resources::WorkError::Admission(_) => ProviderErrorKind::ResourceExhausted,
        rw_resources::WorkError::Worker(_) => ProviderErrorKind::Protocol,
        rw_resources::WorkError::ResultUnavailable => ProviderErrorKind::EffectsUnsettled,
    };
    ProviderError::new(kind, "structured output validation worker did not complete")
}

#[cfg(test)]
mod tests;
