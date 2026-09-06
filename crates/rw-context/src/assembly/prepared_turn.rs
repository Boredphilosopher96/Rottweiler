//! Token metadata is constructed with its immutable payload and cannot drift.
use super::{ContextItem, ContextItemId, ContextItemKind, ContextProvenance};
use crate::LocalTokenEstimator;
use rw_types::Turn;

#[derive(Clone)]
pub struct PreparedTurn {
    turn: Turn,
    tokens: u64,
    blocks: Vec<u64>,
}
impl PreparedTurn {
    #[must_use]
    pub fn new(turn: Turn) -> Self {
        let blocks = turn
            .blocks
            .iter()
            .map(LocalTokenEstimator::block)
            .collect::<Vec<_>>();
        let tokens = blocks
            .iter()
            .fold(4_u64, |total, tokens| total.saturating_add(*tokens));
        Self {
            turn,
            tokens,
            blocks,
        }
    }
    #[must_use]
    pub fn turn(&self) -> &Turn {
        &self.turn
    }
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        self.tokens
    }
    #[must_use]
    pub fn block_tokens(&self, index: usize) -> Option<u64> {
        self.blocks.get(index).copied()
    }
    #[must_use]
    pub fn into_item(self, properties: ContextItemProperties) -> PreparedContextItem {
        PreparedContextItem {
            tokens: self.tokens,
            item: ContextItem {
                id: properties.id,
                kind: properties.kind,
                label: properties.label,
                provenance: properties.provenance,
                pinned: properties.pinned,
                evicted: properties.evicted,
                summarized: properties.summarized,
                pruned: properties.pruned,
                turn: self.turn,
            },
        }
    }
}

/// Selection/presentation fields are independent of the immutable tokenized body.
#[allow(clippy::struct_excessive_bools)]
pub struct ContextItemProperties {
    pub id: ContextItemId,
    pub kind: ContextItemKind,
    pub label: String,
    pub provenance: ContextProvenance,
    pub pinned: bool,
    pub evicted: bool,
    pub summarized: bool,
    pub pruned: bool,
}

pub struct PreparedContextItem {
    pub(super) item: ContextItem,
    pub(super) tokens: u64,
}
impl PreparedContextItem {
    #[must_use]
    pub fn new(item: ContextItem) -> Self {
        Self {
            tokens: LocalTokenEstimator::turn(&item.turn),
            item,
        }
    }
    #[must_use]
    pub fn into_item(self) -> ContextItem {
        self.item
    }
}
