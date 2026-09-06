//! Bounded summary fragments preserve exact source/block/byte continuation.
use super::{CanonicalHistory, HistoryMaterializationLimits, RecoveryError};
use rw_types::{Block, ContextBlockId, Role, Turn, TurnMeta};
use std::io::Write;

pub const MAX_SUMMARY_FRAGMENT_BYTES: usize = 256 * 1024;

/// Offsets refer to the canonical JSON representation of one effective block.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConversationFragmentCursor {
    pub ordinal: u64,
    pub block_index: u32,
    pub byte_offset: u64,
}

/// One provider-neutral summary document. Empty means the source is evicted or empty.
pub struct ConversationFragment {
    pub source: ContextBlockId,
    pub next: Option<ConversationFragmentCursor>,
    pub turn: Option<Turn>,
}

impl CanonicalHistory {
    /// Read one bounded summary fragment using the admitted one-event decoder.
    /// The original IR remains authoritative; framing records its role and block kind.
    /// # Errors
    /// Rejects invalid cursors, oversized pins, invalid source bodies and fragment limits.
    pub fn conversation_fragment(
        &self,
        cursor: ConversationFragmentCursor,
        max_bytes: usize,
    ) -> Result<ConversationFragment, RecoveryError> {
        if !(1024..=MAX_SUMMARY_FRAGMENT_BYTES).contains(&max_bytes) {
            return Err(RecoveryError::Limit("summary fragment byte allowance"));
        }
        let source = self.turn_source(cursor.ordinal)?;
        let item = rw_types::context_source::conversation_item(source.sequence);
        let action = self.context_action(&item)?;
        if action.as_ref().is_some_and(|action| action.pinned) {
            return Err(RecoveryError::Limit(
                "pinned conversation exceeds summary page admission",
            ));
        }
        let identity = ContextBlockId {
            sequence: source.sequence,
            block_index: cursor.block_index,
        };
        if action.is_some() {
            return Ok(ConversationFragment {
                source: identity,
                next: None,
                turn: None,
            });
        }
        let mut turns = self.materialize(
            cursor.ordinal..cursor.ordinal + 1,
            HistoryMaterializationLimits::default(),
        )?;
        let turn = turns
            .pop()
            .ok_or(RecoveryError::Invalid("fragment source turn"))?;
        let Some(block) = turn.blocks.get(cursor.block_index as usize) else {
            if turn.blocks.is_empty() && cursor.block_index == 0 && cursor.byte_offset == 0 {
                return Ok(ConversationFragment {
                    source: identity,
                    next: None,
                    turn: None,
                });
            }
            return Err(RecoveryError::Invalid("fragment block cursor"));
        };
        let pruned =
            matches!(block, Block::ToolResult { .. }) && self.pruned_output(identity)?.is_some();
        let replacement;
        let block = if pruned {
            let Block::ToolResult { id, is_error, .. } = block else {
                unreachable!()
            };
            replacement = Block::ToolResult {
                id: id.clone(),
                is_error: *is_error,
                output: rw_types::ToolOutput::Text {
                    text: rw_context::PRUNED_TOOL_OUTPUT_REPLACEMENT.into(),
                },
            };
            &replacement
        } else {
            block
        };
        let header = format!(
            "Canonical {:?} {} block {}:{}; JSON byte offset {}. Consecutive fragments are one block.\n",
            turn.role,
            kind(block),
            source.sequence.0,
            cursor.block_index,
            cursor.byte_offset
        );
        let payload_limit = max_bytes
            .checked_sub(header.len())
            .filter(|limit| *limit >= 4)
            .ok_or(RecoveryError::Limit("fragment framing allowance"))?;
        let (payload, consumed, complete) =
            fragment_json(block, cursor.byte_offset, payload_limit)?;
        let next = if complete {
            (cursor.block_index as usize + 1 < turn.blocks.len()).then_some(
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
                    .checked_add(consumed as u64)
                    .ok_or(RecoveryError::Limit("fragment byte cursor"))?,
                ..cursor
            })
        };
        let mut text = String::with_capacity(header.len() + payload.len());
        text.push_str(&header);
        text.push_str(&payload);
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

struct WindowWriter {
    skipped: u64,
    offset: u64,
    bytes: Vec<u8>,
    limit: usize,
    total: u64,
}
impl Write for WindowWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.total = self
            .total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("fragment size overflow"))?;
        let skip = usize::try_from(self.offset.saturating_sub(self.skipped))
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        self.skipped += skip as u64;
        let available = &bytes[skip..];
        let take = available.len().min(self.limit - self.bytes.len());
        self.bytes.extend_from_slice(&available[..take]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn fragment_json(
    value: &impl serde::Serialize,
    offset: u64,
    limit: usize,
) -> Result<(String, usize, bool), RecoveryError> {
    let mut writer = WindowWriter {
        skipped: 0,
        offset,
        bytes: Vec::with_capacity(limit),
        limit,
        total: 0,
    };
    serde_json::to_writer(&mut writer, value)?;
    if offset >= writer.total {
        return Err(RecoveryError::Invalid("fragment offset outside block"));
    }
    let valid = match std::str::from_utf8(&writer.bytes) {
        Ok(_) => writer.bytes.len(),
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
    writer.bytes.truncate(valid);
    let complete = offset + valid as u64 == writer.total;
    let text =
        String::from_utf8(writer.bytes).map_err(|_| RecoveryError::Invalid("fragment UTF-8"))?;
    Ok((text, valid, complete))
}

#[cfg(test)]
mod tests {
    use super::fragment_json;
    #[test]
    fn fragments_cover_exact_json_bytes_at_utf8_boundaries() {
        let value = serde_json::json!({"nested":["🙂é\\\"\n".repeat(300), {"id":17}]});
        let expected = serde_json::to_string(&value).expect("JSON");
        let mut offset = 0;
        let mut restored = String::new();
        loop {
            let (part, consumed, complete) = fragment_json(&value, offset, 17).expect("fragment");
            assert!(part.len() <= 17);
            assert!(consumed > 0);
            restored.push_str(&part);
            offset += consumed as u64;
            if complete {
                break;
            }
        }
        assert_eq!(restored, expected);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&restored).expect("JSON"),
            value
        );
    }
}
