use std::sync::Arc;

use async_trait::async_trait;
use rw_context::LocalTokenEstimator;
use rw_providers::ProviderRequest;
use rw_types::AccountingAttribution;
use tokio::sync::mpsc;

use super::{TurnSignal, persist_event};
use crate::engine::{AgentLoopError, PendingEvent, SessionActorConfig, wire_turn_id};
use crate::provider_admission::{
    BudgetReservationError, ProviderAccountingSink, ProviderCallActuals, ProviderCallIdentity,
    ProviderCallReceipt, ProviderInputBudget, ProviderInvocation,
};

pub(super) fn invocation(
    config: &SessionActorConfig,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    attribution: AccountingAttribution,
    request: &ProviderRequest,
) -> Result<ProviderInvocation, AgentLoopError> {
    let input = request
        .turns
        .iter()
        .fold(LocalTokenEstimator::tools(&request.tools), |total, turn| {
            total.saturating_add(LocalTokenEstimator::turn(turn))
        });
    binding(config, signals, turn, attribution, input)
}

pub(super) fn binding(
    config: &SessionActorConfig,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    attribution: AccountingAttribution,
    estimated_input: u64,
) -> Result<ProviderInvocation, AgentLoopError> {
    let mut id = [0_u8; 16];
    getrandom::fill(&mut id).map_err(|error| {
        AgentLoopError::InvalidConfiguration(format!("provider call identity: {error}"))
    })?;
    Ok(ProviderInvocation {
        budget_session_id: config.budget_session_id.clone(),
        session_id: config.session_id.clone(),
        turn_id: wire_turn_id(turn),
        attribution,
        call_id: u128::from_be_bytes(id).to_string(),
        input: ProviderInputBudget::Estimated(estimated_input),
        budget: config.model.budget_config(),
        clock: Arc::clone(&config.event_clock),
        admission: Arc::clone(&config.provider_admission),
        accounting: Arc::new(SessionAccountingSink {
            signals: signals.clone(),
        }),
    })
}

struct SessionAccountingSink {
    signals: mpsc::UnboundedSender<TurnSignal>,
}

#[async_trait]
impl ProviderAccountingSink for SessionAccountingSink {
    async fn append_accounted(
        &self,
        identity: ProviderCallIdentity,
        actuals: ProviderCallActuals,
    ) -> Result<ProviderCallReceipt, BudgetReservationError> {
        let meta = persist_event(
            &self.signals,
            PendingEvent::ProviderCallAccounted {
                call: identity.clone(),
                actuals: actuals.clone(),
            },
        )
        .await
        .map_err(|error| BudgetReservationError::Worker(error.to_string()))?;
        if meta.session_id != identity.session_id {
            return Err(BudgetReservationError::InvalidPlan(
                "accounting receipt session",
            ));
        }
        Ok(ProviderCallReceipt {
            identity,
            sequence_id: meta.sequence_id,
            accounted_at: rw_store::session::UtcTimestamp::parse(meta.emitted_at)?,
            actuals,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rw_types::{Cost, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, TurnId, Usage};

    #[tokio::test]
    async fn accounting_receipt_uses_exact_acknowledged_sequence_and_timestamp() {
        let (signals, mut receive) = mpsc::unbounded_channel();
        let sink = SessionAccountingSink { signals };
        let identity = ProviderCallIdentity {
            budget_session_id: SessionId("receipt-session".into()),
            session_id: SessionId("receipt-session".into()),
            turn_id: TurnId("7".into()),
            attribution: AccountingAttribution::Main,
            call_id: "receipt-call".into(),
            attempt: 2,
        };
        let actuals = ProviderCallActuals {
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Monetary {
                amount_micros: 17,
                currency: "USD".into(),
            },
        };
        let expected_identity = identity.clone();
        let expected_actuals = actuals.clone();
        let append = tokio::spawn(async move { sink.append_accounted(identity, actuals).await });
        let Some(TurnSignal::DurableEvent {
            kind: PendingEvent::ProviderCallAccounted { call, actuals },
            respond,
        }) = receive.recv().await
        else {
            panic!("expected durable accounting event")
        };
        assert_eq!(call, expected_identity);
        assert_eq!(actuals, expected_actuals);
        assert!(!append.is_finished());
        respond
            .send(Ok(EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: call.session_id.clone(),
                sequence_id: SequenceId(123),
                emitted_at: "2026-09-04T04:05:06.123Z".into(),
                caused_by: None,
            }))
            .unwrap();
        let receipt = append.await.unwrap().unwrap();
        assert_eq!(receipt.identity, expected_identity);
        assert_eq!(receipt.actuals, expected_actuals);
        assert_eq!(receipt.sequence_id, SequenceId(123));
        assert_eq!(receipt.accounted_at.as_str(), "2026-09-04T04:05:06.123Z");
    }
}
