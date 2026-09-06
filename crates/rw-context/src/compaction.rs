//! ADR-010 compaction planning and summary placement.

use rw_types::{Block, Role, ToolOutput, ToolOutputPart, Turn, TurnMeta};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Exact synthetic continuation text required after automatic compaction.
pub const AUTO_CONTINUE_TEXT: &str =
    "Continue if you have next steps, or stop and ask for clarification";

/// Default summary prompt; extensions may inject context or replace it.
pub const DEFAULT_COMPACTION_PROMPT: &str = r"Create a hand-off summary of the conversation in the user's language. Preserve concrete decisions, constraints, unresolved work, commands, errors, and file state so another agent can continue without the original transcript. Do not call tools. Do not include media. Return only this structure:

## Goal

## Instructions

## Discoveries

## Accomplished

## Relevant files & directories";

/// Why compaction was requested.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    AutomaticOverflow,
    Manual,
    ProviderOverflow,
}

/// Conversation-resident pin with a stable user-visible order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConversationPin {
    pub item_id: String,
    pub order: u64,
    pub turn: Turn,
}

/// Result of the extension `pre_compact` hook.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreCompactHook {
    /// Context strings appended to the chosen summary prompt.
    pub injected_context: Vec<String>,
    /// When present, replaces the default prompt before injection.
    pub replacement_prompt: Option<String>,
}

/// Pure inputs to compaction planning.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionInput {
    pub conversation: Vec<Turn>,
    pub pins: Vec<ConversationPin>,
    pub reason: CompactionReason,
    pub instructions: Option<String>,
    pub hook: PreCompactHook,
    pub session_model_alias: String,
    pub compaction_model_alias: Option<String>,
    pub automatic_continue: bool,
}

/// A compaction model request plan and deterministic post-summary placement.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPlan {
    /// Media-free history passed to the compaction agent.
    pub history: Vec<Turn>,
    pub summary_prompt: String,
    pub model_alias: String,
    /// Compaction requests never expose tools.
    pub expose_tools: bool,
    pub reason: CompactionReason,
    pub ordered_pins: Vec<ConversationPin>,
    /// Last real user turn replayed only for provider-overflow recovery.
    pub replay_user: Option<Turn>,
    pub auto_continue: Option<Turn>,
}

/// Compaction planning errors.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CompactionError {
    #[error("provider-overflow compaction needs a real user turn to replay")]
    MissingReplayUser,
    #[error("session model alias cannot be empty")]
    MissingModelAlias,
}

/// Pure ADR-010 compaction planner.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Compactor;

impl Compactor {
    /// Produces a model-call plan without making a call or mutating history.
    ///
    /// # Errors
    ///
    /// Rejects an empty model alias or provider-overflow input without a real
    /// user turn to replay.
    pub fn plan(mut input: CompactionInput) -> Result<CompactionPlan, CompactionError> {
        if input.session_model_alias.trim().is_empty() {
            return Err(CompactionError::MissingModelAlias);
        }
        input
            .pins
            .sort_by(|left, right| (left.order, &left.item_id).cmp(&(right.order, &right.item_id)));

        let (history, replay_user) = if input.reason == CompactionReason::ProviderOverflow {
            let replay_index = input
                .conversation
                .iter()
                .rposition(is_real_user)
                .ok_or(CompactionError::MissingReplayUser)?;
            let replay_user = sanitize_history(&input.conversation[replay_index..=replay_index])
                .into_iter()
                .next()
                .ok_or(CompactionError::MissingReplayUser)?;
            (
                sanitize_history(&input.conversation[..replay_index]),
                Some(replay_user),
            )
        } else {
            (sanitize_history(&input.conversation), None)
        };

        let mut summary_prompt = input
            .hook
            .replacement_prompt
            .unwrap_or_else(|| DEFAULT_COMPACTION_PROMPT.to_owned());
        if let Some(instructions) = input.instructions.filter(|value| !value.trim().is_empty()) {
            summary_prompt.push_str("\n\nAdditional user instructions:\n");
            summary_prompt.push_str(&instructions);
        }
        for injected in input
            .hook
            .injected_context
            .iter()
            .filter(|value| !value.trim().is_empty())
        {
            summary_prompt.push_str("\n\nInjected context:\n");
            summary_prompt.push_str(injected);
        }

        let auto_continue = (input.reason == CompactionReason::AutomaticOverflow
            && input.automatic_continue)
            .then(auto_continue_turn);
        Ok(CompactionPlan {
            history,
            summary_prompt,
            model_alias: input
                .compaction_model_alias
                .filter(|alias| !alias.trim().is_empty())
                .unwrap_or(input.session_model_alias),
            expose_tools: false,
            reason: input.reason,
            ordered_pins: input.pins,
            replay_user,
            auto_continue,
        })
    }
}

impl CompactionPlan {
    /// Places summary, pins, replayed user, then auto-continue.
    #[must_use]
    pub fn post_summary_turns(&self, summary: impl Into<String>) -> Vec<Turn> {
        let mut turns = vec![summary_turn(summary)];
        turns.extend(self.ordered_pins.iter().map(|pin| pin.turn.clone()));
        turns.extend(self.replay_user.iter().cloned());
        turns.extend(self.auto_continue.iter().cloned());
        turns
    }
}

/// Wraps model summary text as an in-conversation assistant message.
#[must_use]
pub fn summary_turn(summary: impl Into<String>) -> Turn {
    Turn {
        role: Role::Assistant,
        blocks: vec![Block::Text {
            text: summary.into(),
        }],
        meta: TurnMeta {
            synthetic: true,
            summary: true,
            ..TurnMeta::default()
        },
    }
}

/// Synthetic continuation appended once after a complete automatic summary.
#[must_use]
pub fn auto_continue_turn() -> Turn {
    Turn {
        role: Role::User,
        blocks: vec![Block::Text {
            text: AUTO_CONTINUE_TEXT.to_owned(),
        }],
        meta: TurnMeta {
            synthetic: true,
            ..TurnMeta::default()
        },
    }
}

fn is_real_user(turn: &Turn) -> bool {
    turn.role == Role::User && !turn.meta.synthetic
}

fn sanitize_history(turns: &[Turn]) -> Vec<Turn> {
    turns
        .iter()
        .cloned()
        .map(|mut turn| {
            turn.blocks = turn.blocks.into_iter().map(sanitize_block).collect();
            turn
        })
        .collect()
}

fn sanitize_block(block: Block) -> Block {
    match block {
        Block::Image { media_type, .. } => Block::Text {
            text: format!("[Image omitted during compaction: {media_type}]"),
        },
        Block::ToolResult {
            id,
            output: ToolOutput::Mixed { parts },
            is_error,
        } => Block::ToolResult {
            id,
            output: ToolOutput::Mixed {
                parts: parts
                    .into_iter()
                    .map(|part| match part {
                        ToolOutputPart::Image { media_type, .. } => ToolOutputPart::Text {
                            text: format!("[Tool image omitted during compaction: {media_type}]"),
                        },
                        other => other,
                    })
                    .collect(),
            },
            is_error,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use rw_types::{Block, ImageRef, Role, Turn, TurnMeta};

    use super::{
        AUTO_CONTINUE_TEXT, CompactionInput, CompactionReason, Compactor, ConversationPin,
        PreCompactHook,
    };

    fn turn(role: Role, text: &str) -> Turn {
        Turn {
            role,
            blocks: vec![Block::Text { text: text.into() }],
            meta: TurnMeta::default(),
        }
    }

    fn input(reason: CompactionReason) -> CompactionInput {
        CompactionInput {
            conversation: vec![turn(Role::User, "question")],
            pins: Vec::new(),
            reason,
            instructions: None,
            hook: PreCompactHook::default(),
            session_model_alias: "fast".into(),
            compaction_model_alias: None,
            automatic_continue: true,
        }
    }

    #[test]
    fn provider_overflow_replays_last_user_intact_after_pins() {
        let mut input = input(CompactionReason::ProviderOverflow);
        input.conversation[0].blocks.push(Block::Image {
            media_type: "image/png".into(),
            data: ImageRef::InlineBase64 {
                data: "AA==".into(),
            },
        });
        input
            .conversation
            .push(turn(Role::Assistant, "partial response must be dropped"));
        input.pins = vec![
            ConversationPin {
                item_id: "later".into(),
                order: 2,
                turn: turn(Role::User, "pin 2"),
            },
            ConversationPin {
                item_id: "earlier".into(),
                order: 1,
                turn: turn(Role::User, "pin 1"),
            },
        ];
        let plan = Compactor::plan(input);
        assert!(plan.as_ref().is_ok_and(|value| value.history.is_empty()));
        let post = plan.map(|value| value.post_summary_turns("summary"));
        assert_eq!(post.as_ref().map(Vec::len), Ok(4));
        assert!(post.is_ok_and(|turns| {
            matches!(&turns[3].blocks[0], Block::Text { text } if text == "question")
                && matches!(&turns[3].blocks[1], Block::Text { text } if text.contains("Image omitted"))
                && turns.iter().all(|turn| {
                    !turn.blocks.iter().any(|block| {
                        matches!(block, Block::Text { text } if text.contains("partial response"))
                    })
                })
        }));
    }

    #[test]
    fn compaction_agent_history_has_no_media_and_no_tools() {
        let mut input = input(CompactionReason::Manual);
        input.conversation[0].blocks.push(Block::Image {
            media_type: "image/jpeg".into(),
            data: ImageRef::Url {
                url: "https://example.invalid/image".into(),
            },
        });
        let plan = Compactor::plan(input);
        assert!(plan.as_ref().is_ok_and(|value| !value.expose_tools));
        assert!(
            plan.is_ok_and(|value| { matches!(value.history[0].blocks[1], Block::Text { .. }) })
        );
    }

    #[test]
    fn automatic_compaction_appends_exact_nudge() {
        let plan = Compactor::plan(input(CompactionReason::AutomaticOverflow));
        assert!(plan.is_ok_and(|value| {
            matches!(
                &value.auto_continue,
                Some(Turn { blocks, .. })
                    if matches!(&blocks[0], Block::Text { text } if text == AUTO_CONTINUE_TEXT)
            )
        }));
    }

    #[test]
    fn one_hundred_fifty_turn_plan_is_deterministic() {
        let mut input = input(CompactionReason::Manual);
        input.conversation = (0..150)
            .map(|index| {
                turn(
                    if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    &format!("turn {index}: src/lib.rs is modified"),
                )
            })
            .collect();
        let first = Compactor::plan(input.clone());
        let second = Compactor::plan(input);
        assert_eq!(first, second);
        assert!(first.is_ok_and(|value| {
            value.history.len() == 150
                && value
                    .summary_prompt
                    .contains("## Relevant files & directories")
        }));
    }
}
