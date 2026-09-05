//! Small actor metadata derived while a worker owns the conversation materialization.
use rw_types::{ContextItemId, Role, Turn};

#[derive(Clone, Default)]
pub(in crate::engine) struct ConversationSummary {
    pub turns: u64,
    pub system_turns: u64,
    pub resolved_model: Option<String>,
    pub system_resolved_model: Option<String>,
    pub title_prompt: Option<String>,
    pub has_assistant_text: bool,
    pub approved_plan_item: Option<ContextItemId>,
}

impl ConversationSummary {
    pub(in crate::engine) fn from_turns(turns: &[Turn]) -> Self {
        let resolved = |turn: &Turn| {
            turn.meta
                .model
                .as_ref()
                .filter(|model| model.contains('/'))
                .cloned()
        };
        Self {
            turns: turns.len() as u64,
            system_turns: turns
                .iter()
                .filter(|turn| turn.role == Role::System)
                .count() as u64,
            resolved_model: turns.iter().rev().find_map(resolved),
            system_resolved_model: turns
                .iter()
                .rev()
                .filter(|turn| turn.role == Role::System)
                .find_map(resolved),
            title_prompt: crate::engine::turn::title::first_meaningful_user_prompt(turns),
            has_assistant_text: crate::engine::turn::title::has_successful_assistant_text(turns),
            approved_plan_item: crate::engine::projection::approved_plan_context_item(turns),
        }
    }
}
