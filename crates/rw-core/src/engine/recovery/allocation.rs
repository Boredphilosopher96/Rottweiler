//! Retained result allocations are independent of a source decoder's scratch.
use super::{ConversationPage, RecoveryBootstrap, RecoveryError, RecoveryHead};
use rw_types::allocation::PrepareAllocation;
use std::mem::size_of;

/// One admitted canonical result, including its payload and selector metadata.
pub const MAX_HISTORY_RESULT_BYTES: usize = 128 * 1024 * 1024;

fn checked(value: Option<usize>) -> Result<usize, RecoveryError> {
    value
        .filter(|bytes| *bytes <= MAX_HISTORY_RESULT_BYTES)
        .ok_or(RecoveryError::Limit("retained canonical result allocation"))
}
fn heap(value: &impl PrepareAllocation) -> Option<usize> {
    value.prepared_heap_bytes()
}
fn vector<T>(value: &Vec<T>, item: impl Fn(&T) -> Option<usize>) -> Option<usize> {
    value.iter().try_fold(
        value.capacity().checked_mul(size_of::<T>())?,
        |bytes, value| bytes.checked_add(item(value)?),
    )
}
fn sum(values: impl IntoIterator<Item = Option<usize>>) -> Option<usize> {
    values
        .into_iter()
        .try_fold(0_usize, |total, value| total.checked_add(value?))
}
impl ConversationPage {
    /// Measure the normalized turns and all returned source/mutation metadata.
    /// # Errors
    /// Rejects overflow or a result exceeding the canonical result ceiling.
    pub fn retained_bytes(&self) -> Result<usize, RecoveryError> {
        checked(sum([
            Some(size_of::<Self>()),
            heap(&self.turns),
            vector(&self.sources, |_| Some(0)),
            vector(&self.context_actions, |action| {
                action
                    .as_ref()
                    .map_or(Some(0), |action| heap(&action.item_id))
            }),
            heap(&self.pruned_tool_outputs),
        ]))
    }
}
impl RecoveryBootstrap {
    /// Measure retained controls and repairs before transferring their allowance.
    /// Raw decoder pages and their scratch have a separate worker allowance.
    /// # Errors
    /// Rejects overflow or a result exceeding the canonical result ceiling.
    pub fn retained_bytes(&self) -> Result<usize, RecoveryError> {
        // Control source charges cover the decoded values and their cloned fields.
        // Explicit vector storage covers the wrapper rows added by the projection.
        let controls = &self.controls;
        let repairs = self.interrupted.as_ref().map_or(Some(0), |repair| {
            sum([
                heap(&repair.tool_turn),
                heap(&repair.assistant_turn),
                vector(&repair.tools, |tool| {
                    sum([
                        heap(&tool.tool_call_id),
                        heap(&tool.invocation_id),
                        heap(&tool.output),
                        tool.missing_start.as_ref().map_or(Some(0), |start| {
                            sum([heap(&start.name), heap(&start.arguments)])
                        }),
                    ])
                }),
            ])
        });
        checked(sum([
            Some(size_of::<Self>()),
            head_heap(&self.head),
            usize::try_from(controls.decoded_bytes).ok(),
            vector(&controls.queued_messages, |_| Some(0)),
            vector(&controls.accepted_messages, |_| Some(0)),
            vector(&controls.pending_questions, |_| Some(0)),
            repairs,
        ]))
    }

    /// Normalize generic repaired tool outputs under an already reserved result allowance.
    pub fn prepare_allocations(&mut self) {
        if let Some(repair) = &mut self.interrupted {
            repair.tool_turn.prepare_allocations();
            repair.assistant_turn.prepare_allocations();
            for tool in &mut repair.tools {
                tool.output.prepare_allocations();
                if let Some(start) = &mut tool.missing_start {
                    start.arguments.prepare_allocations();
                }
            }
        }
    }
}
fn head_heap(head: &RecoveryHead) -> Option<usize> {
    let control = &head.control;
    sum([
        heap(&head.session_id),
        head.accounting.retained_heap_bytes(),
        heap(&control.driver),
        heap(&control.mode_id),
        control
            .active_shell
            .as_ref()
            .map_or(Some(0), |(id, _)| heap(id)),
        vector(&control.queued, |_| Some(0)),
        vector(&control.accepted, |_| Some(0)),
        vector(&control.questions, |question| heap(&question.id)),
    ])
}

/// Admit selector and mutation maps before allocating them. Every possible tool
/// result is charged, including those with no current pruning entry.
pub(super) fn admit_page_metadata(turns: &Vec<rw_types::Turn>) -> Result<(), RecoveryError> {
    let mut count = 0_usize;
    let mut key_bytes = 0_usize;
    for turn in turns {
        for block in &turn.blocks {
            if let rw_types::Block::ToolResult { id, .. } = block {
                count = count
                    .checked_add(1)
                    .ok_or(RecoveryError::Limit("tool selector count"))?;
                key_bytes = key_bytes
                    .checked_add(id.0.len())
                    .ok_or(RecoveryError::Limit("tool selector bytes"))?;
            }
        }
    }
    let empty = std::collections::BTreeMap::<String, u64>::new();
    let metadata_per_turn = size_of::<super::ConversationSource>()
        + size_of::<Option<crate::engine::ContextSurgeryAction>>()
        + rw_types::extension_control::MAX_CONTEXT_ITEM_ID_BYTES;
    checked(sum([
        Some(size_of::<ConversationPage>()),
        heap(turns),
        turns.len().checked_mul(metadata_per_turn),
        count
            .checked_add(1)
            .and_then(|count| heap(&empty)?.checked_mul(count)),
        Some(key_bytes),
    ]))
    .map(|_| ())
}
