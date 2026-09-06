//! Request cache owned by the context working lease, never by the session history.
use super::context::prompt_turn;
use crate::engine::recovery::ConversationSource;
use rw_context::{PreparedPrefix, ToonPromptEncoder};
use rw_providers::CacheBreakpointSupport;
use rw_types::{Block, SequenceId, Turn};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct ContextCache {
    turns: BTreeMap<u64, NormalizedTurn>,
    #[cfg(test)]
    pub normalizations: u64,
    pub prefix: Option<(CacheBreakpointSupport, PreparedPrefix)>,
}
struct NormalizedTurn {
    pruned: Vec<u32>,
    incoming: ToonPromptEncoder,
    outgoing: ToonPromptEncoder,
    turn: Turn,
}
impl ContextCache {
    /// Sources identify immutable durable values. A changed prune selection or
    /// incoming format-note state invalidates normalization at that exact source.
    pub fn conversation(
        &mut self,
        conversation: &[Turn],
        sources: &[ConversationSource],
        pruned: &BTreeMap<String, u64>,
    ) -> Vec<Turn> {
        // Logical order need not be physical sequence order after prefix replacement.
        let selected = sources
            .iter()
            .map(|source| source.sequence.0)
            .collect::<BTreeSet<_>>();
        self.turns.retain(|sequence, _| selected.contains(sequence));
        let mut toon = ToonPromptEncoder::default();
        conversation
            .iter()
            .zip(sources)
            .map(|(turn, source)| self.turn(turn, source.sequence, pruned, &mut toon))
            .collect()
    }

    fn turn(
        &mut self,
        turn: &Turn,
        sequence: SequenceId,
        pruned: &BTreeMap<String, u64>,
        toon: &mut ToonPromptEncoder,
    ) -> Turn {
        let selected = turn
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                let index = u32::try_from(index).ok()?;
                (matches!(block, Block::ToolResult { .. })
                    && pruned.contains_key(
                        &rw_types::ContextBlockId {
                            sequence,
                            block_index: index,
                        }
                        .key(),
                    ))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if let Some(entry) = self.turns.get(&sequence.0)
            && entry.pruned == selected
            && entry.incoming == *toon
        {
            *toon = entry.outgoing;
            return entry.turn.clone();
        }
        // Remove before constructing replacement so invalidated output is not
        // retained alongside its replacement and encoder workspace.
        self.turns.remove(&sequence.0);
        #[cfg(test)]
        {
            self.normalizations += 1;
        }
        let incoming = *toon;
        let normalized = prompt_turn(turn, sequence, pruned, toon);
        self.turns.insert(
            sequence.0,
            NormalizedTurn {
                pruned: selected,
                incoming,
                outgoing: *toon,
                turn: normalized.clone(),
            },
        );
        normalized
    }
}
