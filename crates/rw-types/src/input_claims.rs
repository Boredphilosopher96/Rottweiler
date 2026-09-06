//! Bounded input claims folded from a contiguous, source-qualified durable prefix.
use crate::{
    EngineEvent, SequenceId, SessionId, TurnStatus, session_state::MAX_SESSION_QUEUE_ITEMS,
};
use rw_memory_derive::PrepareAllocation as Allocation;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, Allocation)]
#[serde(deny_unknown_fields)]
pub struct AcceptedSource {
    pub agent_turn: u64,
    pub claimed_turn: u64,
    pub sequence: SequenceId,
    pub retained: bool,
    pub ended: bool,
}

/// A projection checkpoint retains identities only, never accepted input bodies.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, Allocation)]
#[serde(deny_unknown_fields)]
pub struct InputClaimState {
    #[serde(deserialize_with = "Option::deserialize")]
    session: Option<SessionId>,
    next_sequence: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    active: Option<u64>,
    #[serde(deserialize_with = "decode_pending")]
    pending: Vec<AcceptedSource>,
}

/// Borrows the exact event checked against the state's preceding watermark.
pub struct InputClaimChecked<'a> {
    event: &'a EngineEvent,
}
impl<'a> InputClaimChecked<'a> {
    #[must_use]
    pub fn event(&self) -> &'a EngineEvent {
        self.event
    }
}
impl InputClaimState {
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }
    #[must_use]
    pub fn pending(&self) -> &[AcceptedSource] {
        &self.pending
    }
    /// Abandon pending interaction when selecting a completed rewind boundary.
    pub fn abandon_pending(&mut self) {
        self.pending.clear();
        self.active = None;
    }
    /// Check one exact next event before transferring its input authority.
    /// # Errors
    /// Rejects noncontiguous/foreign events, oversized state, invalid claim phases,
    /// duplicate consumption and input references owned by a different attempt.
    pub fn advance<'a>(
        &mut self,
        event: &'a EngineEvent,
    ) -> Result<InputClaimChecked<'a>, &'static str> {
        let meta = event.meta().ok_or("input claim source must be durable")?;
        if meta.protocol_version != crate::PROTOCOL_VERSION
            || meta.sequence_id.0 != self.next_sequence
            || self
                .session
                .as_ref()
                .is_some_and(|session| session != &meta.session_id)
            || self.pending.len() > MAX_SESSION_QUEUE_ITEMS
        {
            return Err("input claim checkpoint/source identity");
        }
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or("input claim sequence overflow")?;
        self.transition(event)?;
        self.session = Some(meta.session_id.clone());
        self.next_sequence = next;
        Ok(InputClaimChecked { event })
    }
    fn transition(&mut self, event: &EngineEvent) -> Result<(), &'static str> {
        match event {
            EngineEvent::TurnStarted { turn_id, .. } => self.start(turn(turn_id)?)?,
            EngineEvent::UserMessageAccepted {
                meta, agent_turn, ..
            } => {
                if self.pending.len() >= MAX_SESSION_QUEUE_ITEMS {
                    return Err("accepted message identities");
                }
                self.pending.push(AcceptedSource {
                    agent_turn: *agent_turn,
                    claimed_turn: *agent_turn,
                    sequence: meta.sequence_id,
                    retained: false,
                    ended: false,
                });
            }
            EngineEvent::UserMessageRetained {
                accepted_source, ..
            } => {
                let input = self
                    .pending
                    .iter_mut()
                    .find(|input| input.sequence == *accepted_source)
                    .ok_or("retained input must be pending")?;
                if input.retained || input.ended || self.active != Some(input.claimed_turn) {
                    return Err("input retention source or phase");
                }
                input.retained = true;
            }
            EngineEvent::ConversationInputCommitted {
                accepted_source,
                agent_turn,
                ..
            } => {
                let index = self
                    .pending
                    .iter()
                    .position(|input| input.sequence == *accepted_source)
                    .ok_or("input is not pending")?;
                let input = &self.pending[index];
                if input.claimed_turn != *agent_turn
                    || input.retained
                    || input.ended
                    || (input.agent_turn != *agent_turn && self.active != Some(*agent_turn))
                {
                    return Err("input commit must own its active claim");
                }
                self.pending.remove(index);
            }
            EngineEvent::TurnFinished {
                turn_id, status, ..
            } => self.finish(turn(turn_id)?, status)?,
            EngineEvent::ConversationRewound { .. } => self.abandon_pending(),
            _ => {}
        }
        Ok(())
    }
    fn start(&mut self, turn: u64) -> Result<(), &'static str> {
        if self.pending.iter().any(|input| {
            input.retained && (self.active.is_some() || !input.ended || turn <= input.claimed_turn)
        }) {
            return Err("retained input claim requires an ended turn");
        }
        for input in self.pending.iter_mut().filter(|input| input.retained) {
            input.claimed_turn = turn;
            input.retained = false;
            input.ended = false;
        }
        self.active = Some(turn);
        Ok(())
    }
    fn finish(&mut self, turn: u64, status: &TurnStatus) -> Result<(), &'static str> {
        if *status != TurnStatus::Interrupted
            && self
                .pending
                .iter()
                .any(|input| input.claimed_turn == turn && input.retained)
        {
            return Err("retained input requires interrupted closure");
        }
        self.pending
            .retain(|input| input.claimed_turn != turn || input.retained);
        for input in self
            .pending
            .iter_mut()
            .filter(|input| input.claimed_turn == turn)
        {
            input.ended = true;
        }
        if self.active == Some(turn) {
            self.active = None;
        }
        Ok(())
    }
}
fn turn(id: &crate::TurnId) -> Result<u64, &'static str> {
    let turn =
        id.0.parse::<u64>()
            .map_err(|_| "input claim turn identity")?;
    if turn.to_string() != id.0 {
        return Err("input claim turn identity");
    }
    Ok(turn)
}

pub const MAX_INPUT_CLAIM_CHECKPOINT_BYTES: usize = 32 * 1024;
impl InputClaimState {
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }
    /// Check the bounded state against its enclosing published source identity.
    /// # Errors
    /// Rejects partial, foreign or oversized checkpoints.
    pub fn validate_checkpoint(&self, next: u64, session: &str) -> Result<(), &'static str> {
        if session.len() > 128
            || self.next_sequence != next
            || self.pending.len() > MAX_SESSION_QUEUE_ITEMS
            || (next == 0 && (self.session.is_some() || self.active.is_some()))
            || self
                .pending
                .windows(2)
                .any(|pair| pair[0].sequence >= pair[1].sequence)
            || (next > 0 && self.session.as_ref().is_none_or(|id| id.0 != session))
            || self.pending.iter().any(|input| {
                input.sequence.0 >= next
                    || input.agent_turn > input.claimed_turn
                    || (input.ended && !input.retained)
            })
        {
            return Err("input checkpoint source watermark");
        }
        Ok(())
    }
}

fn decode_pending<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> Result<Vec<AcceptedSource>, D::Error> {
    struct Pending;
    impl<'de> serde::de::Visitor<'de> for Pending {
        type Value = Vec<AcceptedSource>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded list of pending accepted sources")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut pending = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_SESSION_QUEUE_ITEMS),
            );
            while pending.len() < MAX_SESSION_QUEUE_ITEMS {
                let Some(input) = sequence.next_element()? else {
                    return Ok(pending);
                };
                pending.push(input);
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom("accepted message identities"));
            }
            Ok(pending)
        }
    }
    decoder.deserialize_seq(Pending)
}
