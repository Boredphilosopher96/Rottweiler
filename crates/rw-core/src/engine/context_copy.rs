//! Fresh JSON map copies under an already-acquired prepared-allocation plan.
//!
//! Callers must validate `PrepareAllocation::prepared_bytes` before entry. That
//! bounds recursive depth and accounts fresh map nodes without inheriting opaque
//! spare hash-table capacity from an admitted source.
use rw_types::{Block, ImageRef, ToolOutput, ToolOutputPart, Turn, TurnMeta};
use serde_json::Value;

pub(super) fn turn_with_policy_slot(source: &Turn) -> Turn {
    let mut blocks = Vec::with_capacity(source.blocks.len() + 1);
    blocks.extend(source.blocks.iter().map(block));
    Turn {
        role: source.role.clone(),
        blocks,
        meta: TurnMeta {
            created_at: source.meta.created_at.clone(),
            model: source.meta.model.clone(),
            synthetic: source.meta.synthetic,
            summary: source.meta.summary,
        },
    }
}
fn block(source: &Block) -> Block {
    match source {
        Block::Text { text } => Block::Text { text: text.clone() },
        Block::Thinking { content, signature } => Block::Thinking {
            content: content.clone(),
            signature: signature.clone(),
        },
        Block::ToolCall { id, name, args } => Block::ToolCall {
            id: id.clone(),
            name: name.clone(),
            args: json(args),
        },
        Block::ToolResult {
            id,
            output,
            is_error,
        } => Block::ToolResult {
            id: id.clone(),
            output: tool_output(output),
            is_error: *is_error,
        },
        Block::Image { media_type, data } => Block::Image {
            media_type: media_type.clone(),
            data: image(data),
        },
        Block::Citation {
            uri,
            title,
            excerpt,
        } => Block::Citation {
            uri: uri.clone(),
            title: title.clone(),
            excerpt: excerpt.clone(),
        },
    }
}
fn image(source: &ImageRef) -> ImageRef {
    match source {
        ImageRef::InlineBase64 { data } => ImageRef::InlineBase64 { data: data.clone() },
        ImageRef::Url { url } => ImageRef::Url { url: url.clone() },
    }
}
fn tool_output(source: &ToolOutput) -> ToolOutput {
    match source {
        ToolOutput::Text { text } => ToolOutput::Text { text: text.clone() },
        ToolOutput::Structured { value } => ToolOutput::Structured { value: json(value) },
        ToolOutput::Mixed { parts } => ToolOutput::Mixed {
            parts: parts.iter().map(part).collect(),
        },
    }
}
fn part(source: &ToolOutputPart) -> ToolOutputPart {
    match source {
        ToolOutputPart::Text { text } => ToolOutputPart::Text { text: text.clone() },
        ToolOutputPart::Structured { value } => ToolOutputPart::Structured { value: json(value) },
        ToolOutputPart::Image { media_type, data } => ToolOutputPart::Image {
            media_type: media_type.clone(),
            data: image(data),
        },
    }
}

pub(super) fn json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(json).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), json(value)))
                .collect(),
        ),
        Value::Null => Value::Null,
        Value::Bool(value) => Value::Bool(*value),
        Value::Number(value) => Value::Number(value.clone()),
        Value::String(value) => Value::String(value.clone()),
    }
}
