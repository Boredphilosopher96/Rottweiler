//! Inputs and legal decisions for the engine's hook phases.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::{CompactionReason, ToolCapability, ToolOutput, TurnStatus};

pub const HOOK_PHASE_TIMEOUT_MS: u64 = 5_000;
pub const HOOK_SETTLEMENT_TIMEOUT_MS: u64 = 2_000;
pub const MAX_HOOKS_PER_EVENT: usize = 128;
pub const MAX_HOOK_DIAGNOSTIC_BYTES: usize = 4_096;

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, JsonSchema, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreTool,
    PostTool,
    PreCompact,
    TurnEnd,
    PermissionCheck,
}

impl HookEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
            Self::PreCompact => "pre_compact",
            Self::TurnEnd => "turn_end",
            Self::PermissionCheck => "permission_check",
        }
    }

    #[must_use]
    pub const fn accepts_transform(self) -> bool {
        matches!(
            self,
            Self::UserPromptSubmit | Self::PreTool | Self::PostTool | Self::PreCompact
        )
    }
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, JsonSchema, TS,
)]
#[serde(rename_all = "kebab-case")]
pub enum HookFailurePolicy {
    FailOpen,
    FailClosed,
}

/// Class order ensures policy observes the transformed input.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, JsonSchema, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum HookClass {
    Transform,
    Policy,
    Observer,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(
    tag = "hook",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HookInput {
    SessionStart(HookSessionInput),
    SessionEnd(HookSessionInput),
    UserPromptSubmit(HookPromptInput),
    PreTool(HookToolInput),
    PostTool(HookToolResultInput),
    PreCompact(HookCompactionInput),
    TurnEnd(HookTurnInput),
    PermissionCheck(HookPermissionInput),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HookSessionInput {
    pub session_id: String,
    pub workspace: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HookPromptInput {
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HookToolInput {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HookToolResultInput {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub output: ToolOutput,
    pub is_error: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HookPermissionInput {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub capabilities: Vec<ToolCapability>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HookCompactionInput {
    pub reason: CompactionReason,
    pub conversation_turns: u32,
    pub injected_context: Vec<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    pub replacement_prompt: Option<String>,
    pub suppress_auto_continue: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HookTurnInput {
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub turn: u64,
    pub status: TurnStatus,
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, JsonSchema, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum HookPermissionDecision {
    Allow,
    Ask,
    Deny,
}

/// Transformations expose mutable fields only; invocation identity is immutable.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(tag = "hook", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookTransform {
    UserPromptSubmit {
        content: String,
    },
    PreTool {
        name: String,
        arguments: Value,
    },
    PostTool {
        output: ToolOutput,
        is_error: bool,
    },
    PreCompact {
        injected_context: Vec<String>,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
        replacement_prompt: Option<String>,
        suppress_auto_continue: bool,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema, TS)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookDirective {
    Continue {},
    Transform { change: HookTransform },
    Permission { value: HookPermissionDecision },
    Block { message: String },
}

impl HookInput {
    #[must_use]
    pub const fn event(&self) -> HookEvent {
        match self {
            Self::SessionStart(_) => HookEvent::SessionStart,
            Self::SessionEnd(_) => HookEvent::SessionEnd,
            Self::UserPromptSubmit(_) => HookEvent::UserPromptSubmit,
            Self::PreTool(_) => HookEvent::PreTool,
            Self::PostTool(_) => HookEvent::PostTool,
            Self::PreCompact(_) => HookEvent::PreCompact,
            Self::TurnEnd(_) => HookEvent::TurnEnd,
            Self::PermissionCheck(_) => HookEvent::PermissionCheck,
        }
    }

    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::PreTool(input) => Some(&input.name),
            Self::PostTool(input) => Some(&input.name),
            Self::PermissionCheck(input) => Some(&input.name),
            _ => None,
        }
    }

    /// Applies a transform only to its matching phase.
    ///
    /// # Errors
    /// Rejects a mismatched phase or invalid tool name without changing the input.
    pub fn apply(&mut self, change: HookTransform) -> Result<(), &'static str> {
        match (self, change) {
            (Self::UserPromptSubmit(input), HookTransform::UserPromptSubmit { content }) => {
                input.content = content;
            }
            (Self::PreTool(input), HookTransform::PreTool { name, arguments }) => {
                if name.is_empty()
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    })
                {
                    return Err("hook transform has an invalid tool name");
                }
                input.name = name;
                input.arguments = arguments;
            }
            (Self::PostTool(input), HookTransform::PostTool { output, is_error }) => {
                input.output = output;
                input.is_error |= is_error;
            }
            (
                Self::PreCompact(input),
                HookTransform::PreCompact {
                    injected_context,
                    replacement_prompt,
                    suppress_auto_continue,
                },
            ) => {
                input.injected_context = injected_context;
                input.replacement_prompt = replacement_prompt;
                input.suppress_auto_continue = suppress_auto_continue;
            }
            _ => return Err("hook transform does not match its phase"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
