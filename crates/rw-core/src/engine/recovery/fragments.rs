//! One admitted source encoding supplies every bounded summary continuation.
use super::{CanonicalHistory, HistoryMaterializationLimits, RecoveryError};
use rw_types::{Block, ContextBlockId, Role, SequenceId, Turn, TurnMeta};
use std::{io::Write, ops::Range};

pub const MAX_SUMMARY_FRAGMENT_BYTES: usize = 256 * 1024;
const MAX_ENCODED_SOURCE_BYTES: usize = 32 * 1024 * 1024;

/// Offsets refer to the canonical JSON representation of one effective block.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConversationFragmentCursor {
    pub ordinal: u64,
    pub block_index: u32,
    pub byte_offset: u64,
}
pub struct ConversationFragment {
    pub source: ContextBlockId,
    pub next: Option<ConversationFragmentCursor>,
    pub turn: Option<Turn>,
}
struct EncodedBlock {
    kind: &'static str,
    range: Range<usize>,
}
/// A single immutable source, encoded once after pruning. Its admitted owner
/// remains with the caller until every continuation and provider request settles.
pub struct ConversationFragmentSource {
    ordinal: u64,
    sequence: SequenceId,
    role: Role,
    bytes: Vec<u8>,
    blocks: Vec<EncodedBlock>,
}
impl CanonicalHistory {
    /// # Errors
    /// Rejects oversized pins, source decode admission, and invalid effective blocks.
    pub fn conversation_fragment_source(
        &self,
        ordinal: u64,
    ) -> Result<ConversationFragmentSource, RecoveryError> {
        let source = self.turn_source(ordinal)?;
        let action = self.context_action(&rw_types::context_source::conversation_item(
            source.sequence,
        ))?;
        if action.as_ref().is_some_and(|action| action.pinned) {
            return Err(RecoveryError::Limit(
                "pinned conversation exceeds summary page admission",
            ));
        }
        if action.is_some() {
            return Ok(ConversationFragmentSource {
                ordinal,
                sequence: source.sequence,
                role: Role::User,
                bytes: Vec::new(),
                blocks: Vec::new(),
            });
        }
        let mut turns = self.materialize(
            ordinal
                ..ordinal
                    .checked_add(1)
                    .ok_or(RecoveryError::Limit("fragment ordinal"))?,
            HistoryMaterializationLimits::default(),
        )?;
        let mut turn = turns
            .pop()
            .ok_or(RecoveryError::Invalid("fragment source turn"))?;
        for (index, block) in turn.blocks.iter_mut().enumerate() {
            let identity = ContextBlockId {
                sequence: source.sequence,
                block_index: u32::try_from(index)
                    .map_err(|_| RecoveryError::Limit("fragment block index"))?,
            };
            if matches!(block, Block::ToolResult { .. }) && self.pruned_output(identity)?.is_some()
            {
                let Block::ToolResult { output, .. } = block else {
                    unreachable!()
                };
                *output = rw_types::ToolOutput::Text {
                    text: rw_context::PRUNED_TOOL_OUTPUT_REPLACEMENT.into(),
                };
            }
        }
        ConversationFragmentSource::encode(ordinal, source.sequence, turn)
    }
}
impl ConversationFragmentSource {
    fn encode(ordinal: u64, sequence: SequenceId, turn: Turn) -> Result<Self, RecoveryError> {
        let mut writer = SourceWriter(Vec::new());
        let mut blocks = Vec::with_capacity(turn.blocks.len());
        for block in &turn.blocks {
            let start = writer.0.len();
            serde_json::to_writer(&mut writer, block)?;
            blocks.push(EncodedBlock {
                kind: kind(block),
                range: start..writer.0.len(),
            });
        }
        Ok(Self {
            ordinal,
            sequence,
            role: turn.role,
            bytes: writer.0,
            blocks,
        })
    }
    /// Capacity, not just string length, stays charged through every fragment.
    #[must_use]
    pub fn retained_bytes(&self) -> Option<usize> {
        self.blocks
            .capacity()
            .checked_mul(std::mem::size_of::<EncodedBlock>())?
            .checked_add(self.bytes.capacity())?
            .checked_add(std::mem::size_of::<Self>())
    }
    /// # Errors
    /// Rejects foreign sources, gaps, non-UTF-8 offsets, and inadmissible fragment sizes.
    pub fn fragment(
        &self,
        cursor: ConversationFragmentCursor,
        max_bytes: usize,
    ) -> Result<ConversationFragment, RecoveryError> {
        if cursor.ordinal != self.ordinal {
            return Err(RecoveryError::Invalid("fragment source ordinal"));
        }
        if !(256..=MAX_SUMMARY_FRAGMENT_BYTES).contains(&max_bytes) {
            return Err(RecoveryError::Limit("summary fragment byte allowance"));
        }
        let identity = ContextBlockId {
            sequence: self.sequence,
            block_index: cursor.block_index,
        };
        let Some(block) = self.blocks.get(cursor.block_index as usize) else {
            if self.blocks.is_empty() && cursor.block_index == 0 && cursor.byte_offset == 0 {
                return Ok(ConversationFragment {
                    source: identity,
                    next: None,
                    turn: None,
                });
            }
            return Err(RecoveryError::Invalid("fragment block cursor"));
        };
        let header = format!(
            "Canonical {:?} {} block {}:{}; JSON byte offset {}. Consecutive fragments are one block.\n",
            self.role, block.kind, self.sequence.0, cursor.block_index, cursor.byte_offset
        );
        let limit = max_bytes
            .checked_sub(header.len())
            .filter(|limit| *limit >= 4)
            .ok_or(RecoveryError::Limit("fragment framing allowance"))?;
        let bytes = &self.bytes[block.range.clone()];
        let (payload, complete) = json_window(bytes, cursor.byte_offset, limit)?;
        let next = if complete {
            (cursor.block_index as usize + 1 < self.blocks.len()).then_some(
                ConversationFragmentCursor {
                    ordinal: cursor.ordinal,
                    block_index: cursor
                        .block_index
                        .checked_add(1)
                        .ok_or(RecoveryError::Limit("fragment block index"))?,
                    byte_offset: 0,
                },
            )
        } else {
            Some(ConversationFragmentCursor {
                byte_offset: cursor
                    .byte_offset
                    .checked_add(payload.len() as u64)
                    .ok_or(RecoveryError::Limit("fragment byte cursor"))?,
                ..cursor
            })
        };
        let mut text = String::with_capacity(header.len() + payload.len());
        text.push_str(&header);
        text.push_str(payload);
        Ok(ConversationFragment {
            source: identity,
            next,
            turn: Some(Turn {
                role: Role::User,
                blocks: vec![Block::Text { text }],
                meta: TurnMeta::default(),
            }),
        })
    }
}
fn kind(block: &Block) -> &'static str {
    match block {
        Block::Text { .. } => "text",
        Block::Thinking { .. } => "thinking",
        Block::ToolCall { .. } => "tool_call",
        Block::ToolResult { .. } => "tool_result",
        Block::Image { .. } => "image",
        Block::Citation { .. } => "citation",
    }
}
struct SourceWriter(Vec<u8>);
impl Write for SourceWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let total = self
            .0
            .len()
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_ENCODED_SOURCE_BYTES)
            .ok_or_else(|| std::io::Error::other("summary source encoded admission"))?;
        if total > self.0.capacity() {
            let capacity = total
                .next_power_of_two()
                .max(4096)
                .min(MAX_ENCODED_SOURCE_BYTES);
            self.0
                .try_reserve_exact(capacity - self.0.len())
                .map_err(std::io::Error::other)?;
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn json_window(bytes: &[u8], offset: u64, limit: usize) -> Result<(&str, bool), RecoveryError> {
    let offset = usize::try_from(offset).map_err(|_| RecoveryError::Invalid("fragment offset"))?;
    if offset >= bytes.len() {
        return Err(RecoveryError::Invalid("fragment offset outside block"));
    }
    let end = offset.saturating_add(limit).min(bytes.len());
    let candidate = &bytes[offset..end];
    let valid = match std::str::from_utf8(candidate) {
        Ok(_) => candidate.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => {
            return Err(RecoveryError::Invalid(
                "fragment offset is not a UTF-8 boundary",
            ));
        }
    };
    if valid == 0 {
        return Err(RecoveryError::Limit("fragment cannot advance"));
    }
    let text = std::str::from_utf8(&candidate[..valid])
        .map_err(|_| RecoveryError::Invalid("fragment UTF-8"))?;
    Ok((text, offset + valid == bytes.len()))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::json_window;
    #[test]
    fn one_encoded_source_supplies_all_continuations_without_reencoding() {
        use super::{ConversationFragmentCursor, ConversationFragmentSource};
        use rw_types::{Block, Role, SequenceId, Turn, TurnMeta};
        let block = Block::Text {
            text: "🙂é retained facts\n".repeat(2000),
        };
        let expected = serde_json::to_string(&block).expect("expected bytes");
        let source = ConversationFragmentSource::encode(
            7,
            SequenceId(41),
            Turn {
                role: Role::Assistant,
                blocks: vec![block],
                meta: TurnMeta::default(),
            },
        )
        .expect("one encoding");
        let allocation = source.bytes.as_ptr();
        let capacity = source.retained_bytes();
        let mut cursor = ConversationFragmentCursor {
            ordinal: 7,
            block_index: 0,
            byte_offset: 0,
        };
        let mut joined = String::new();
        let mut parts = 0;
        loop {
            let part = source.fragment(cursor, 256).expect("next source window");
            assert_eq!(part.source.sequence, SequenceId(41));
            let turn = part.turn.expect("fragment body");
            let Block::Text { text } = &turn.blocks[0] else {
                panic!("text framing")
            };
            assert!(text.len() <= 256);
            joined.push_str(text.split_once('\n').expect("framing").1);
            parts += 1;
            assert_eq!(source.bytes.as_ptr(), allocation);
            assert_eq!(source.retained_bytes(), capacity);
            let Some(next) = part.next else {
                break;
            };
            assert!(next.byte_offset > cursor.byte_offset);
            cursor = next;
        }
        assert!(parts > 100);
        assert_eq!(joined, expected);
        assert!(
            source
                .fragment(
                    ConversationFragmentCursor {
                        ordinal: 8,
                        ..cursor
                    },
                    256
                )
                .is_err()
        );
    }
    #[test]
    fn fragments_cover_exact_json_bytes_at_utf8_boundaries() {
        let value = serde_json::json!({"nested":["🙂é\\\"\n".repeat(300), {"id":17}]});
        let encoded = serde_json::to_vec(&value).expect("JSON");
        let mut offset = 0;
        let mut restored = String::new();
        loop {
            let (part, complete) = json_window(&encoded, offset, 17).expect("fragment");
            assert!(part.len() <= 17 && !part.is_empty());
            restored.push_str(part);
            offset += part.len() as u64;
            if complete {
                break;
            }
        }
        assert_eq!(restored.as_bytes(), encoded);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&restored).expect("JSON"),
            value
        );
    }
}
