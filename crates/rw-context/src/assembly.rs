//! Stable-prefix context assembly with provenance and cache metadata.

mod prepared;
mod prepared_turn;
pub use prepared::PreparedPrefix;
pub use prepared_turn::{ContextItemProperties, PreparedContextItem, PreparedTurn};

use std::collections::HashSet;

use rw_providers::{CacheBreakpointSupport, ToolDefinition};
use rw_types::{Block, Role, Turn};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::estimate::{LocalTokenEstimator, canonicalize_json};

const PREFIX_HASH_DOMAIN: &[u8] = b"rottweiler.context.stable-prefix.v1\0";

/// Stable identity for one assembled context item.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ContextItemId(pub String);

/// Semantic placement of a context item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextItemKind {
    /// Top-level system/developer instructions.
    System,
    /// Project-local instructions, such as `AGENTS.md`.
    ProjectInstructions,
    /// Compact skill/plugin discovery metadata.
    SkillIndex,
    /// Persisted conversation history.
    Conversation,
    /// Conversation-resident user pin.
    Pin,
    /// User input queued while a run is active.
    Queued,
}

impl ContextItemKind {
    fn allowed_in_stable_prefix(self) -> bool {
        matches!(
            self,
            Self::System | Self::ProjectInstructions | Self::SkillIndex
        )
    }
}

/// Where an item came from, retained for inspection and eviction UI.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextProvenance {
    /// Built into the application.
    BuiltIn,
    /// Read from a project instruction file.
    ProjectFile { path: String },
    /// Supplied by an extension.
    Extension { extension_id: String },
    /// Persisted in the current conversation.
    Conversation { sequence: u64 },
    /// Explicitly pinned by the user.
    UserPin,
    /// Queued by a client during a run.
    ClientQueue,
}

/// One independently inspectable and evictable context item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ContextItem {
    pub id: ContextItemId,
    pub kind: ContextItemKind,
    pub label: String,
    pub provenance: ContextProvenance,
    pub turn: Turn,
    /// Pinned items cannot be selected by ordinary eviction policies.
    pub pinned: bool,
    /// Evicted items stay inspectable but are omitted from provider input.
    pub evicted: bool,
    /// Item content was incorporated into a later summary.
    pub summarized: bool,
    /// Tool output was replaced by the deterministic prune marker.
    pub pruned: bool,
}

/// Inputs grouped by their required assembly order.
#[derive(Clone, Debug, PartialEq)]
pub struct AssemblyInput {
    pub stable_prefix: Vec<ContextItem>,
    pub conversation: Vec<ContextItem>,
    pub pins: Vec<ContextItem>,
    pub queued: Vec<ContextItem>,
    pub tools: Vec<ToolDefinition>,
    pub cache_support: CacheBreakpointSupport,
    /// Prompt dumps contain user content and therefore require explicit opt-in.
    pub include_prompt_dump: bool,
}

impl Default for AssemblyInput {
    fn default() -> Self {
        Self {
            stable_prefix: Vec::new(),
            conversation: Vec::new(),
            pins: Vec::new(),
            queued: Vec::new(),
            tools: Vec::new(),
            cache_support: CacheBreakpointSupport::None,
            include_prompt_dump: false,
        }
    }
}

/// Per-item accounting used by context inspectors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ContextItemBreakdown {
    pub id: ContextItemId,
    pub kind: ContextItemKind,
    pub label: String,
    pub provenance: ContextProvenance,
    pub tokens: u64,
    pub pinned: bool,
    pub evicted: bool,
    pub summarized: bool,
    pub pruned: bool,
    /// Present only when the item was included in provider input.
    pub assembled_turn_index: Option<usize>,
}

/// Kind of cache marker represented by a breakpoint descriptor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBreakpointKind {
    /// The adapter must emit an explicit provider marker.
    Explicit,
    /// The provider manages caching, while the hash remains useful to metrics.
    ProviderManaged,
}

/// Cache boundary immediately following the stable prefix.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CacheBreakpoint {
    pub kind: CacheBreakpointKind,
    pub after_turn_count: usize,
    pub prefix_tokens: u64,
    pub stable_prefix_hash: String,
    /// Last stable-prefix item when one exists; tools may still follow it.
    pub after_item_id: Option<ContextItemId>,
}

/// Token accounting split by context region.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenTotals {
    pub stable_prefix: u64,
    pub tools: u64,
    pub conversation: u64,
    pub pins: u64,
    pub queued: u64,
    pub total: u64,
}

/// An opt-in canonical dump suitable for explicit debug/export commands.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromptDump {
    /// Canonical provider-neutral assembled request, not raw provider wire.
    pub assembled_provider_neutral_json: String,
}

/// Provider-ready context plus inspectable accounting metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct AssembledContext {
    pub turns: Vec<Turn>,
    pub tools: Vec<ToolDefinition>,
    pub items: Vec<ContextItemBreakdown>,
    /// Exact canonical provider-neutral bytes covered by the stable boundary.
    /// Only provider-visible roles, blocks, and sorted tool definitions appear.
    pub stable_prefix_bytes: Vec<u8>,
    pub stable_prefix_turn_count: usize,
    pub stable_prefix_hash: String,
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    pub token_totals: TokenTotals,
    pub prompt_dump: Option<PromptDump>,
}

/// Context assembly failures are deterministic input errors.
#[derive(Debug, Error)]
pub enum AssemblyError {
    #[error("duplicate context item id: {0}")]
    DuplicateItemId(String),
    #[error("{0:?} items are not allowed in the stable prefix")]
    InvalidStablePrefixKind(ContextItemKind),
    #[error("duplicate tool name: {0}")]
    DuplicateToolName(String),
    #[error("failed to serialize canonical context: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Pure, deterministic stable-prefix assembler.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextAssembler;

impl ContextAssembler {
    /// Assembles context in the order stable prefix, conversation, pins, queue.
    ///
    /// # Errors
    ///
    /// Rejects duplicate identities/tool names, misplaced stable items, or an
    /// unexpected serialization failure.
    pub fn assemble(input: AssemblyInput) -> Result<AssembledContext, AssemblyError> {
        validate_items(&input)?;
        let input_stable_len = input.stable_prefix.len();

        let mut tools = input.tools;
        tools.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        validate_tools(&tools)?;

        let (stable_prefix_bytes, stable_prefix_hash) =
            stable_prefix_representation(&input.stable_prefix, &tools)?;
        let mut turns = Vec::new();
        let mut breakdown = Vec::new();
        let mut totals = TokenTotals {
            tools: LocalTokenEstimator::tools(&tools),
            ..TokenTotals::default()
        };

        append_region(
            &mut turns,
            &mut breakdown,
            input.stable_prefix,
            &mut totals.stable_prefix,
        );
        let stable_turn_count = turns.len();
        append_region(
            &mut turns,
            &mut breakdown,
            input.conversation,
            &mut totals.conversation,
        );
        append_region(&mut turns, &mut breakdown, input.pins, &mut totals.pins);
        append_region(&mut turns, &mut breakdown, input.queued, &mut totals.queued);
        totals.stable_prefix = totals.stable_prefix.saturating_add(totals.tools);
        totals.total = totals
            .stable_prefix
            .saturating_add(totals.conversation)
            .saturating_add(totals.pins)
            .saturating_add(totals.queued);

        let cache_breakpoints = match input.cache_support {
            CacheBreakpointSupport::None => Vec::new(),
            CacheBreakpointSupport::Explicit | CacheBreakpointSupport::Automatic => {
                vec![CacheBreakpoint {
                    kind: if input.cache_support == CacheBreakpointSupport::Explicit {
                        CacheBreakpointKind::Explicit
                    } else {
                        CacheBreakpointKind::ProviderManaged
                    },
                    after_turn_count: stable_turn_count,
                    prefix_tokens: totals.stable_prefix,
                    stable_prefix_hash: stable_prefix_hash.clone(),
                    after_item_id: breakdown
                        .iter()
                        .take(input_stable_len)
                        .rev()
                        .find(|item| !item.evicted)
                        .map(|item| item.id.clone()),
                }]
            }
        };

        let prompt_dump = input.include_prompt_dump.then(|| {
            let value = json!({"tools": tools, "turns": turns});
            serde_json::to_string_pretty(&canonicalize_json(&value)).map(
                |assembled_provider_neutral_json| PromptDump {
                    assembled_provider_neutral_json,
                },
            )
        });
        let prompt_dump = prompt_dump.transpose()?;

        Ok(AssembledContext {
            turns,
            tools,
            items: breakdown,
            stable_prefix_bytes,
            stable_prefix_turn_count: stable_turn_count,
            stable_prefix_hash,
            cache_breakpoints,
            token_totals: totals,
            prompt_dump,
        })
    }
}

fn validate_items(input: &AssemblyInput) -> Result<(), AssemblyError> {
    let mut seen = HashSet::new();
    for item in input
        .stable_prefix
        .iter()
        .chain(&input.conversation)
        .chain(&input.pins)
        .chain(&input.queued)
    {
        if !seen.insert(&item.id) {
            return Err(AssemblyError::DuplicateItemId(item.id.0.clone()));
        }
    }
    if let Some(item) = input
        .stable_prefix
        .iter()
        .find(|item| !item.kind.allowed_in_stable_prefix())
    {
        return Err(AssemblyError::InvalidStablePrefixKind(item.kind));
    }
    Ok(())
}

fn validate_tools(tools: &[ToolDefinition]) -> Result<(), AssemblyError> {
    for pair in tools.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(AssemblyError::DuplicateToolName(pair[0].name.clone()));
        }
    }
    Ok(())
}

fn append_region(
    turns: &mut Vec<Turn>,
    breakdown: &mut Vec<ContextItemBreakdown>,
    items: Vec<ContextItem>,
    region_tokens: &mut u64,
) {
    for item in items {
        let tokens = LocalTokenEstimator::turn(&item.turn);
        let assembled_turn_index = if item.evicted {
            None
        } else {
            let index = turns.len();
            turns.push(item.turn);
            *region_tokens = region_tokens.saturating_add(tokens);
            Some(index)
        };
        breakdown.push(ContextItemBreakdown {
            id: item.id,
            kind: item.kind,
            label: item.label,
            provenance: item.provenance,
            tokens,
            pinned: item.pinned,
            evicted: item.evicted,
            summarized: item.summarized,
            pruned: item.pruned,
            assembled_turn_index,
        });
    }
}

#[derive(Serialize)]
struct ProviderVisibleTurn<'a> {
    role: &'a Role,
    blocks: &'a [Block],
}

fn stable_prefix_representation(
    stable_prefix: &[ContextItem],
    tools: &[ToolDefinition],
) -> Result<(Vec<u8>, String), serde_json::Error> {
    let turns: Vec<Value> = stable_prefix
        .iter()
        .filter(|item| !item.evicted)
        .map(|item| {
            serde_json::to_value(ProviderVisibleTurn {
                role: &item.turn.role,
                blocks: &item.turn.blocks,
            })
        })
        .collect::<Result<_, _>>()?;
    let tools = tools
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    let canonical = canonicalize_json(&json!({"turns": turns, "tools": tools}));
    let encoded = serde_json::to_vec(&canonical)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(PREFIX_HASH_DOMAIN);
    hasher.update(&encoded);
    Ok((encoded, hasher.finalize().to_hex().to_string()))
}

#[cfg(test)]
mod tests {
    use rw_providers::{CacheBreakpointSupport, ToolDefinition};
    use rw_types::{Block, Role, Turn, TurnMeta};
    use serde_json::json;

    use super::{
        AssemblyInput, ContextAssembler, ContextItem, ContextItemId, ContextItemKind,
        ContextProvenance,
    };

    fn item(id: &str, kind: ContextItemKind, text: &str) -> ContextItem {
        ContextItem {
            id: ContextItemId(id.into()),
            kind,
            label: id.into(),
            provenance: ContextProvenance::BuiltIn,
            turn: Turn {
                role: Role::System,
                blocks: vec![Block::Text { text: text.into() }],
                meta: TurnMeta::default(),
            },
            pinned: false,
            evicted: false,
            summarized: false,
            pruned: false,
        }
    }

    #[test]
    fn twenty_turns_do_not_change_stable_prefix_hash() {
        let stable = item("system", ContextItemKind::System, "be useful");
        let hashes: Vec<_> = (0..20)
            .map(|turn_count| {
                let conversation = (0..turn_count)
                    .map(|index| {
                        item(
                            &format!("turn-{index}"),
                            ContextItemKind::Conversation,
                            &format!("message {index}"),
                        )
                    })
                    .collect();
                ContextAssembler::assemble(AssemblyInput {
                    stable_prefix: vec![stable.clone()],
                    conversation,
                    cache_support: CacheBreakpointSupport::Explicit,
                    ..AssemblyInput::default()
                })
                .map(|assembled| assembled.stable_prefix_hash)
            })
            .collect::<Result<_, _>>()
            .unwrap_or_default();
        assert_eq!(hashes.len(), 20);
        assert!(hashes.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn evicted_item_is_inspectable_but_not_assembled() {
        let mut evicted = item("old", ContextItemKind::Conversation, "old");
        evicted.evicted = true;
        let assembled = ContextAssembler::assemble(AssemblyInput {
            conversation: vec![evicted],
            ..AssemblyInput::default()
        });
        assert!(assembled.as_ref().is_ok_and(|value| value.turns.is_empty()));
        assert!(assembled.as_ref().is_ok_and(|value| value.items.len() == 1));
        assert!(assembled.is_ok_and(|value| value.items[0].assembled_turn_index.is_none()));
    }

    #[test]
    fn prompt_dump_is_opt_in() {
        let normal = ContextAssembler::assemble(AssemblyInput::default());
        assert!(normal.is_ok_and(|value| value.prompt_dump.is_none()));
        let debug = ContextAssembler::assemble(AssemblyInput {
            include_prompt_dump: true,
            ..AssemblyInput::default()
        });
        assert!(debug.is_ok_and(|value| value.prompt_dump.is_some()));
    }

    #[test]
    fn metadata_only_changes_do_not_break_prefix_hash() {
        let original = item("one", ContextItemKind::System, "same bytes");
        let mut relabeled = original.clone();
        relabeled.id = ContextItemId("two".into());
        relabeled.label = "different label".into();
        relabeled.provenance = ContextProvenance::Extension {
            extension_id: "different-source".into(),
        };
        let hash = |stable_prefix| {
            ContextAssembler::assemble(AssemblyInput {
                stable_prefix,
                ..AssemblyInput::default()
            })
            .map(|value| value.stable_prefix_hash)
            .ok()
        };
        assert_eq!(hash(vec![original]), hash(vec![relabeled]));
        assert_ne!(
            hash(vec![item("one", ContextItemKind::System, "same bytes")]),
            hash(vec![item("one", ContextItemKind::System, "changed bytes")])
        );
    }

    #[test]
    fn turn_metadata_only_changes_do_not_break_prefix_hash_or_bytes() {
        let original = item("one", ContextItemKind::System, "same bytes");
        let mut annotated = original.clone();
        annotated.turn.meta = TurnMeta {
            created_at: Some("2026-07-10T12:00:00Z".into()),
            model: Some("provider-local-model".into()),
            synthetic: true,
            summary: true,
        };
        let assemble = |stable_prefix| {
            ContextAssembler::assemble(AssemblyInput {
                stable_prefix,
                ..AssemblyInput::default()
            })
            .map(|value| (value.stable_prefix_hash, value.stable_prefix_bytes))
            .ok()
        };
        assert_eq!(assemble(vec![original]), assemble(vec![annotated]));
    }

    #[test]
    fn provider_visible_role_change_breaks_prefix_hash() {
        let original = item("one", ContextItemKind::System, "same bytes");
        let mut changed = original.clone();
        changed.turn.role = Role::User;
        let assemble = |stable_prefix| {
            ContextAssembler::assemble(AssemblyInput {
                stable_prefix,
                ..AssemblyInput::default()
            })
            .map(|value| value.stable_prefix_hash)
            .ok()
        };
        assert_ne!(assemble(vec![original]), assemble(vec![changed]));
    }

    #[test]
    fn provider_visible_tool_change_breaks_prefix_hash() {
        let hash = |description: &str| {
            ContextAssembler::assemble(AssemblyInput {
                tools: vec![ToolDefinition {
                    name: "search".into(),
                    description: description.into(),
                    input_schema: json!({"type": "object"}),
                }],
                ..AssemblyInput::default()
            })
            .map(|value| value.stable_prefix_hash)
            .ok()
        };
        assert_ne!(hash("search files"), hash("search symbols"));
    }
}
