//! Normalized stream custody and its installed structured-output validator.
use crate::{ProviderError, ProviderEvent};
use futures_core::Stream;
use futures_util::StreamExt;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// Owned normalized events. Validation metadata is minted only by the output owner.
pub struct BoxEventStream {
    inner: Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send + 'static>>,
    pub(crate) output_contract: Option<[u8; 32]>,
}
impl BoxEventStream {
    /// Adopt an unvalidated normalized event producer.
    pub fn new(
        stream: impl Stream<Item = Result<ProviderEvent, ProviderError>> + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(stream),
            output_contract: None,
        }
    }

    /// Qualify only the message's route label while preserving validated output custody.
    #[must_use]
    pub fn with_model_name(mut self, model: String) -> Self {
        let contract = self.output_contract;
        let stream = async_stream::try_stream! {
            while let Some(event) = self.next().await {
                match event? {
                    ProviderEvent::MessageStart { .. } => yield ProviderEvent::MessageStart { model: model.clone() },
                    event => yield event,
                }
            }
        };
        let mut result = Self::new(stream);
        result.output_contract = contract;
        result
    }
}
impl Stream for BoxEventStream {
    type Item = Result<ProviderEvent, ProviderError>;
    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(context)
    }
}
