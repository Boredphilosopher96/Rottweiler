//! Immutable stable-prefix preparation avoids repeated schema hashing and tokenization.
use super::{
    AssembledContext, AssemblyError, AssemblyInput, ContextAssembler, ContextItem, append_region,
};
use rw_providers::{CacheBreakpointSupport, ToolDefinition};
use std::collections::HashSet;

/// Validated provider-visible prefix, tool definitions and exact token/hash metadata.
/// The caller retains allocation admission for this object and each assembled result.
pub struct PreparedPrefix {
    base: AssembledContext,
}
impl PreparedPrefix {
    /// Prepare the immutable prefix once for one model/tool/instruction configuration.
    /// # Errors
    /// Rejects the same invalid prefix and tool identities as full assembly.
    pub fn new(
        stable_prefix: Vec<ContextItem>,
        tools: Vec<ToolDefinition>,
        cache_support: CacheBreakpointSupport,
    ) -> Result<Self, AssemblyError> {
        Ok(Self {
            base: ContextAssembler::assemble(AssemblyInput {
                stable_prefix,
                tools,
                cache_support,
                ..AssemblyInput::default()
            })?,
        })
    }

    /// Append request-local regions without rebuilding the stable prefix.
    /// # Errors
    /// Rejects duplicate item identities across the prefix and all request regions.
    pub fn assemble(
        &self,
        conversation: Vec<ContextItem>,
        pins: Vec<ContextItem>,
        queued: Vec<ContextItem>,
    ) -> Result<AssembledContext, AssemblyError> {
        let mut seen = self
            .base
            .items
            .iter()
            .map(|item| &item.id)
            .collect::<HashSet<_>>();
        for item in conversation.iter().chain(&pins).chain(&queued) {
            if !seen.insert(&item.id) {
                return Err(AssemblyError::DuplicateItemId(item.id.0.clone()));
            }
        }
        drop(seen);
        let mut result = self.base.clone();
        append_region(
            &mut result.turns,
            &mut result.items,
            conversation,
            &mut result.token_totals.conversation,
        );
        append_region(
            &mut result.turns,
            &mut result.items,
            pins,
            &mut result.token_totals.pins,
        );
        append_region(
            &mut result.turns,
            &mut result.items,
            queued,
            &mut result.token_totals.queued,
        );
        result.token_totals.total = result
            .token_totals
            .stable_prefix
            .saturating_add(result.token_totals.conversation)
            .saturating_add(result.token_totals.pins)
            .saturating_add(result.token_totals.queued);
        Ok(result)
    }
}
