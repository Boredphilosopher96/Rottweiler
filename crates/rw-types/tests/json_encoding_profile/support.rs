use rw_types::{
    EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, ToolCallId, ToolInvocationId,
    ToolOutput, TurnId,
};
use std::io::{self, Write};

pub fn corpus() -> Vec<EngineEvent> {
    let meta = EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("json-profile".into()),
        sequence_id: SequenceId(0),
        emitted_at: "2026-01-01T00:00:00Z".into(),
        caused_by: None,
    };
    let turn_id = TurnId("turn".into());
    let mut events = vec![EngineEvent::TurnStarted {
        meta: meta.clone(),
        turn_id: turn_id.clone(),
    }];
    for text in [
        "delta".to_owned(),
        "Unicode 🦀 λ\n\0\\\"".repeat(64),
        "large tool output\n".repeat(4096),
    ] {
        events.push(EngineEvent::TextDelta {
            meta: meta.clone(),
            turn_id: turn_id.clone(),
            text,
        });
    }
    let structured = serde_json::json!({
        "rows": (0..128).map(|index| serde_json::json!({
            "path": format!("src/module_{index}.rs"), "line": index,
            "matches": ["escaped\ntext", "λ", "\0"], "changed": index % 2 == 0,
        })).collect::<Vec<_>>(),
    });
    events.push(EngineEvent::ToolCallStarted {
        meta: meta.clone(),
        turn_id: turn_id.clone(),
        tool_call_id: ToolCallId("call".into()),
        invocation_id: ToolInvocationId("invocation".into()),
        name: "Search".into(),
        args: serde_json::json!({"query":"λ", "paths":["src", "tests"]}),
        call_index: 0,
    });
    events.push(EngineEvent::ToolCallFinished {
        meta,
        turn_id,
        tool_call_id: ToolCallId("call".into()),
        invocation_id: ToolInvocationId("invocation".into()),
        output: ToolOutput::Structured { value: structured },
        presentation: None,
        is_error: false,
        call_index: 0,
    });
    events
}

/// A concrete count sink with the same pre-write byte ceiling.
#[derive(Default)]
pub struct ReferenceCount(pub usize);
impl Write for ReferenceCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|n| *n <= super::LIMIT)
            .ok_or_else(|| io::Error::other("count limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A concrete buffer sink using the host event allocation policy.
#[derive(Default)]
pub struct ReferenceBuffer(pub Vec<u8>);
impl Write for ReferenceBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .0
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= super::LIMIT)
            .ok_or_else(|| io::Error::other("buffer limit"))?;
        if length > self.0.capacity() {
            let target = self
                .0
                .capacity()
                .max(1024)
                .saturating_mul(2)
                .max(length)
                .min(super::LIMIT);
            self.0
                .try_reserve_exact(target - self.0.len())
                .map_err(io::Error::other)?;
            if self.0.capacity() > super::LIMIT {
                return Err(io::Error::other("buffer capacity"));
            }
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
