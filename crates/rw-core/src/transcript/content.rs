//! Complete semantic bodies are prepared once, then read through bounded slices.

use super::TranscriptProjectionError;
use rw_types::transcript::{
    TranscriptContentSelector, TranscriptContentSource, TranscriptPreviewFormat,
};
use rw_types::{Block, EngineEvent, Role, ToolOutput, Turn};
use serde::{Serialize, Serializer, ser::SerializeSeq};
use std::io::{self, Write};

/// One owned canonical body. Runtime admission must charge its retained capacity.
pub struct TranscriptDocument {
    text: String,
    format: TranscriptPreviewFormat,
}

/// Borrowed UTF-8 slice; the document remains its allocation owner.
#[derive(Debug, Eq, PartialEq)]
pub struct TranscriptDocumentChunk<'a> {
    pub text: &'a str,
    pub next_offset: Option<usize>,
}

impl TranscriptDocument {
    /// Select displayable content from an already bounded, authenticated source.
    ///
    /// # Errors
    /// Rejects a wrong source sequence, selector, body limit, or hidden IR block.
    pub fn from_event(
        event: EngineEvent,
        source: &TranscriptContentSource,
        max_bytes: usize,
    ) -> Result<Self, TranscriptProjectionError> {
        if event
            .meta()
            .is_none_or(|meta| meta.sequence_id != source.sequence)
        {
            return Err(invalid("content source sequence"));
        }
        match (&source.selector, event) {
            (
                TranscriptContentSelector::Conversation {},
                EngineEvent::ConversationTurnCommitted { turn, .. },
            ) => {
                if turn.role == Role::Tool {
                    return Err(invalid("hidden tool conversation"));
                }
                Self::json(&DisplayTurn(&turn), max_bytes)
            }
            (
                TranscriptContentSelector::ConversationBlock { index },
                EngineEvent::ConversationTurnCommitted { turn, .. },
            ) => {
                if turn.role == Role::Tool {
                    return Err(invalid("hidden tool conversation"));
                }
                let block = turn
                    .blocks
                    .into_iter()
                    .nth(*index as usize)
                    .ok_or_else(|| invalid("content block index"))?;
                match block {
                    Block::Text { text } | Block::Thinking { content: text, .. } => {
                        Self::text(text, max_bytes)
                    }
                    Block::Image { .. } | Block::Citation { .. } => Self::json(&block, max_bytes),
                    Block::ToolCall { .. } | Block::ToolResult { .. } => {
                        Err(invalid("hidden tool IR block"))
                    }
                }
            }
            (
                TranscriptContentSelector::ToolArguments {},
                EngineEvent::ToolCallStarted { args, .. },
            ) => Self::json(&args, max_bytes),
            (
                TranscriptContentSelector::ToolOutput {},
                EngineEvent::ToolCallFinished { output, .. },
            ) => match output {
                ToolOutput::Text { text } => Self::text(text, max_bytes),
                _ => Self::json(&output, max_bytes),
            },
            (
                TranscriptContentSelector::ToolPresentation { invocation_id },
                EngineEvent::ToolCallFinished {
                    invocation_id: recorded,
                    presentation: Some(presentation),
                    ..
                },
            ) if invocation_id == &recorded => Self::json(&presentation, max_bytes),
            (TranscriptContentSelector::ToolDiff {}, EngineEvent::ToolDiffReady { diff, .. }) => {
                Self::json(&diff, max_bytes)
            }
            (
                TranscriptContentSelector::CommandMessage {},
                EngineEvent::CommandFinished { message, .. },
            ) => Self::text(message, max_bytes),
            (
                TranscriptContentSelector::ShellCommand {},
                EngineEvent::UserShellStateChanged {
                    command: Some(command),
                    ..
                },
            ) => Self::text(command, max_bytes),
            (
                TranscriptContentSelector::ShellOutput {},
                EngineEvent::UserShellStateChanged {
                    captured_output: Some(output),
                    ..
                },
            ) => Self::text(output, max_bytes),
            (
                TranscriptContentSelector::SubagentTask {},
                EngineEvent::SubagentSpawned { task, .. },
            ) => Self::text(task, max_bytes),
            (
                TranscriptContentSelector::SubagentResult {},
                EngineEvent::SubagentFinished { result, .. },
            ) => Self::text(result.final_text, max_bytes),
            (
                TranscriptContentSelector::SubagentDiff {},
                EngineEvent::SubagentFinished { result, .. },
            ) => Self::text(
                result
                    .diff_artifact
                    .ok_or_else(|| invalid("child has no patch"))?
                    .unified_diff,
                max_bytes,
            ),
            _ => Err(invalid("content selector does not match source")),
        }
    }

    /// Allocation charge, including spare capacity, rather than only string length.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.text.capacity()
    }

    #[must_use]
    pub fn format(&self) -> TranscriptPreviewFormat {
        self.format.clone()
    }

    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.text.len()
    }

    /// Read a bounded borrowed slice without flattening, decoding or rescanning.
    ///
    /// # Errors
    /// Rejects an invalid UTF-8 offset or a limit too small for progress.
    pub fn chunk(
        &self,
        offset: usize,
        max_bytes: usize,
    ) -> Result<TranscriptDocumentChunk<'_>, TranscriptProjectionError> {
        if !self.text.is_char_boundary(offset) || max_bytes == 0 {
            return Err(invalid("content chunk boundary"));
        }
        let mut end = offset.saturating_add(max_bytes).min(self.text.len());
        while !self.text.is_char_boundary(end) {
            end -= 1;
        }
        if end == offset && offset < self.text.len() {
            return Err(invalid("content chunk cannot progress"));
        }
        Ok(TranscriptDocumentChunk {
            text: &self.text[offset..end],
            next_offset: (end < self.text.len()).then_some(end),
        })
    }

    fn text(text: String, max_bytes: usize) -> Result<Self, TranscriptProjectionError> {
        if text.capacity() > max_bytes {
            return Err(invalid("content allocation limit"));
        }
        Ok(Self {
            text,
            format: TranscriptPreviewFormat::Text,
        })
    }

    fn json(value: &impl Serialize, max_bytes: usize) -> Result<Self, TranscriptProjectionError> {
        let mut writer = CappedJson {
            bytes: Vec::new(),
            max_bytes,
        };
        serde_json::to_writer(&mut writer, value)?;
        let text = String::from_utf8(writer.bytes)
            .map_err(|_| invalid("serialized content is not UTF-8"))?;
        Ok(Self {
            text,
            format: TranscriptPreviewFormat::Json,
        })
    }
}

struct CappedJson {
    bytes: Vec<u8>,
    max_bytes: usize,
}
impl Write for CappedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let needed = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|needed| *needed <= self.max_bytes)
            .ok_or_else(|| io::Error::other("content allocation limit"))?;
        if needed > self.bytes.capacity() {
            let target = self
                .bytes
                .capacity()
                .max(4096)
                .saturating_mul(2)
                .max(needed)
                .min(self.max_bytes);
            self.bytes.reserve_exact(target - self.bytes.len());
            if self.bytes.capacity() > self.max_bytes {
                return Err(io::Error::other("content allocation limit"));
            }
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DisplayTurn<'a>(&'a Turn);
impl Serialize for DisplayTurn<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Display<'a> {
            role: &'a Role,
            blocks: DisplayBlocks<'a>,
        }
        Display {
            role: &self.0.role,
            blocks: DisplayBlocks(&self.0.blocks),
        }
        .serialize(serializer)
    }
}
struct DisplayBlocks<'a>(&'a [Block]);
impl Serialize for DisplayBlocks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for block in self.0 {
            match block {
                Block::ToolCall { .. } | Block::ToolResult { .. } => {}
                Block::Thinking { content, .. } => {
                    #[derive(Serialize)]
                    struct Reasoning<'a> {
                        r#type: &'static str,
                        content: &'a str,
                    }
                    sequence.serialize_element(&Reasoning {
                        r#type: "reasoning",
                        content,
                    })?;
                }
                _ => sequence.serialize_element(block)?,
            }
        }
        sequence.end()
    }
}

fn invalid(reason: &'static str) -> TranscriptProjectionError {
    TranscriptProjectionError::Invalid(reason)
}
