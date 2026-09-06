//! Test source scripts distinguish accepted input from already durable facts.
#![cfg(test)]
use crate::engine::PendingEvent;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{Block, Role, SequenceId, Turn, TurnMeta, conversation_input::InputSelection};

pub(super) enum SourceEvent {
    Input { agent_turn: u64, turn: Turn },
    Event(Box<PendingEvent>),
}
impl SourceEvent {
    pub(super) fn event(event: PendingEvent) -> Self {
        Self::Event(Box::new(event))
    }
}
/// Each returned sequence identifies the script item's actual durable commit.
/// Acceptance and its reference remain in the same source batch.
pub(super) fn append_script(
    journal: &mut SegmentedJournal,
    script: Vec<SourceEvent>,
) -> Vec<SequenceId> {
    let first = journal.read_view().prefix_identity().next_sequence;
    let mut pending = Vec::new();
    let mut identities = Vec::with_capacity(script.len());
    for item in script {
        match item {
            SourceEvent::Input { agent_turn, turn } => {
                assert_eq!(turn.role, Role::User, "accepted input role");
                assert_eq!(turn.meta, TurnMeta::default(), "accepted input metadata");
                let [Block::Text { text }] = turn.blocks.as_slice() else {
                    panic!("attachment fixtures require explicit StoredAttachment input")
                };
                let accepted_source = SequenceId(first + pending.len() as u64);
                pending.push(PendingEvent::UserMessageAccepted {
                    turn: agent_turn,
                    content: text.clone(),
                    attachments: vec![],
                });
                pending.push(PendingEvent::ConversationInputCommitted {
                    agent_turn,
                    accepted_source,
                    selection: InputSelection::Accepted {},
                });
            }
            SourceEvent::Event(event) => pending.push(*event),
        }
        identities.push(SequenceId(first + pending.len() as u64 - 1));
    }
    super::tests::append(journal, pending);
    identities
}
