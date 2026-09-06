//! Context assembly, budgeting, compaction, pruning, compact encodings, and spend controls.

pub mod assembly;
pub mod budget;
pub mod cache;
pub mod compaction;
pub mod estimate;
pub mod prune;
pub mod spend;
pub mod toon;

pub use assembly::{
    AssembledContext, AssemblyError, AssemblyInput, CacheBreakpoint, CacheBreakpointKind,
    ContextAssembler, ContextItem, ContextItemBreakdown, ContextItemId, ContextItemKind,
    ContextProvenance, PromptDump, TokenTotals,
};
pub use budget::{
    BudgetEstimate, BudgetSnapshot, Budgeter, InvalidBudgetSnapshot, OverflowDecision,
    OverflowPolicy, OverflowPolicyError, Reconciliation,
};
pub use cache::{CacheObservation, CacheRuleProfile, CacheSimulation, CacheSimulator};
pub use compaction::{
    AUTO_CONTINUE_TEXT, CompactionError, CompactionInput, CompactionPlan, CompactionReason,
    Compactor, ConversationPin, DEFAULT_COMPACTION_PROMPT, PreCompactHook, auto_continue_turn,
    summary_turn,
};
pub use estimate::{LocalTokenEstimator, canonicalize_json};
pub use prune::{
    DEFAULT_MINIMUM_RECLAIM_TOKENS, DEFAULT_PROTECTED_TOOL_TOKENS, DEFAULT_RECENT_USER_TURNS,
    PRUNED_TOOL_OUTPUT_REPLACEMENT, PruneConfig, PruneDecision, PrunePlan, PruneRecord,
    PruneRecordKind, PruneStopReason, Pruner,
};
pub use spend::{
    SpendAmount, SpendAttribution, SpendCaps, SpendCompleteness, SpendConfigError, SpendEntry,
    SpendSignal, SpendSignalKind, SpendStatus, SpendTracker, SpendUnit,
};
pub use toon::{
    EncodedToon, TOON_FORMAT_NOTE, ToonError, ToonPromptEncoder, decode as decode_toon,
    encode as encode_toon,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "context";
