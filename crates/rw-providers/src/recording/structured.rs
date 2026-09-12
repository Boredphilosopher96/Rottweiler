//! Reconcile the validated normalized result while preserving raw wire fidelity.
use super::{RecordedItem, raw_replay_mismatch};
use crate::{BoxEventStream, ProviderErrorKind};
use futures_util::StreamExt;

pub(super) fn reconcile(mut source: BoxEventStream, recorded: Vec<RecordedItem>) -> BoxEventStream {
    let fingerprint = source.output_contract;
    let stream = async_stream::try_stream! {
        let mut expected = recorded.into_iter();
        let mut terminal = None;
        while let Some(actual) = source.next().await {
            let next = expected.next().ok_or_else(raw_replay_mismatch)?;
            match (actual, next) {
                (Ok(actual), RecordedItem::Event { event }) if actual == event => {
                    if matches!(actual, crate::ProviderEvent::Finished { .. }) { terminal = Some(actual); } else { yield actual; }
                },
                (Err(actual), RecordedItem::Error { error }) if actual == error => Err(error)?,
                (Err(actual), RecordedItem::Error { error })
                    if super::is_incomplete_replay_error(&actual)
                        && (error.is_retryable() || error.kind == ProviderErrorKind::Cancelled)
                        && expected.len() == 0 => Err(error)?,
                _ => Err(raw_replay_mismatch())?,
            }
        }
        if expected.next().is_some() { Err(raw_replay_mismatch())?; }
        if let Some(terminal) = terminal { yield terminal; }
    };
    let mut result = BoxEventStream::new(stream);
    result.output_contract = fingerprint;
    result
}
