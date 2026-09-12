//! Failure equality retains fixed digests, never arguments or result bodies.
use super::tool_requests::{PendingToolCall, ToolExecution};
use rw_types::{
    ToolOutput,
    json_encoding::JsonWriter,
    tool_admission::{MAX_PENDING_TOOL_ARGUMENT_BYTES, MAX_TOOL_NAME_BYTES},
    tool_result_admission::MAX_TOOL_RESULT_IR_BYTES,
};
use serde::Serialize;
use std::collections::VecDeque;

// Each component already has source admission; include JSON field/envelope bytes.
const MAX_SIGNATURE_BYTES: usize =
    MAX_TOOL_RESULT_IR_BYTES + MAX_PENDING_TOOL_ARGUMENT_BYTES + MAX_TOOL_NAME_BYTES + 64;

pub(super) struct DoomLoopGuard {
    threshold: usize,
    recent_failures: VecDeque<Option<blake3::Hash>>,
    window_capacity: usize,
}

impl DoomLoopGuard {
    pub(super) fn new(threshold: usize) -> Self {
        Self {
            threshold,
            recent_failures: VecDeque::new(),
            window_capacity: threshold.saturating_mul(4),
        }
    }

    pub(super) fn observe(&mut self, call: &PendingToolCall, result: &ToolExecution) -> bool {
        let signature = result
            .is_error
            .then(|| signature(call, &result.output))
            .flatten();
        self.recent_failures.push_back(signature);
        while self.recent_failures.len() > self.window_capacity {
            self.recent_failures.pop_front();
        }
        signature.is_some_and(|signature| {
            self.recent_failures
                .iter()
                .flatten()
                .filter(|recent| **recent == signature)
                .count()
                >= self.threshold
        })
    }
}

#[derive(Serialize)]
struct Failure<'a> {
    name: &'a str,
    arguments: &'a Option<serde_json::Value>,
    output: &'a ToolOutput,
}

fn signature(call: &PendingToolCall, output: &ToolOutput) -> Option<blake3::Hash> {
    let mut digest = blake3::Hasher::new();
    JsonWriter::stream(&mut digest, MAX_SIGNATURE_BYTES)
        .serialize(&Failure {
            name: &call.name,
            arguments: &call.arguments,
            output,
        })
        .ok()?;
    Some(digest.finalize())
}

#[cfg(test)]
mod tests;
