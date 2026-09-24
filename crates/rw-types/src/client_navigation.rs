//! Engine-owned catalog of built-in interactive commands.
//!
//! This table is the single source for slash discovery, the command palette,
//! engine registration, and generated client metadata. Clients own only
//! presentation: section order, titles, argument hints, keybinding ids,
//! availability links, and visibility rules all originate here.
use serde::Serialize;

use crate::SessionActionKind;

/// Discovery section in canonical display order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum CommandSection {
    #[serde(rename = "Conversation")]
    Conversation,
    #[serde(rename = "Models & agents")]
    ModelsAndAgents,
    #[serde(rename = "Context & usage")]
    ContextAndUsage,
    #[serde(rename = "Workspace")]
    Workspace,
    #[serde(rename = "Safety")]
    Safety,
    #[serde(rename = "Settings & help")]
    SettingsAndHelp,
}

/// Sections in display order. Extension commands follow these in clients.
pub const COMMAND_SECTIONS: &[CommandSection] = &[
    CommandSection::Conversation,
    CommandSection::ModelsAndAgents,
    CommandSection::ContextAndUsage,
    CommandSection::Workspace,
    CommandSection::Safety,
    CommandSection::SettingsAndHelp,
];

/// Session state that must hold before a client lists an entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandVisibility {
    Always,
    /// At least one message waits in the session queue.
    QueuedMessages,
    /// This session has or had child agents.
    ChildAgents,
    /// The client retains at least one error for this session.
    Errors,
}

/// Which owner handles an invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandTarget {
    /// An interactive client opens its screen and accepts no arguments.
    /// Headless peers receive the engine handler's response.
    Screen,
    /// Without arguments an interactive client opens its screen; arguments run
    /// the engine handler.
    ScreenOrEngine,
    /// The engine always handles the invocation.
    Engine,
}

/// Which host layer registers the engine handler for an entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandRegistrar {
    /// Registered by `rw-core` with every session actor.
    Core,
    /// Registered by the runtime that owns the backing resource.
    Runtime,
}

/// One built-in command descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CommandCatalogEntry {
    /// Canonical slash name without the leading `/`.
    pub name: &'static str,
    /// Alternate slash names that resolve to this entry.
    pub aliases: &'static [&'static str],
    pub title: &'static str,
    pub section: CommandSection,
    /// One-line description.
    pub description: &'static str,
    /// Argument syntax, or empty when the command takes none.
    pub argument_hint: &'static str,
    /// Client keybinding action id that runs the same entry.
    pub keybinding: Option<&'static str>,
    /// Engine-projected readiness for the action this entry performs.
    pub availability: Option<SessionActionKind>,
    pub visibility: CommandVisibility,
    pub target: CommandTarget,
    #[serde(skip)]
    pub registrar: CommandRegistrar,
}

impl CommandCatalogEntry {
    /// Canonical usage copy, including the argument hint when present.
    #[must_use]
    pub fn usage(&self) -> String {
        if self.argument_hint.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.argument_hint)
        }
    }
}

const fn entry(
    name: &'static str,
    title: &'static str,
    section: CommandSection,
    description: &'static str,
) -> CommandCatalogEntry {
    CommandCatalogEntry {
        name,
        aliases: &[],
        title,
        section,
        description,
        argument_hint: "",
        keybinding: None,
        availability: None,
        visibility: CommandVisibility::Always,
        target: CommandTarget::Screen,
        registrar: CommandRegistrar::Core,
    }
}

impl CommandCatalogEntry {
    const fn aliases(mut self, aliases: &'static [&'static str]) -> Self {
        self.aliases = aliases;
        self
    }
    const fn arguments(mut self, hint: &'static str) -> Self {
        self.argument_hint = hint;
        self
    }
    const fn key(mut self, action: &'static str) -> Self {
        self.keybinding = Some(action);
        self
    }
    const fn availability(mut self, action: SessionActionKind) -> Self {
        self.availability = Some(action);
        self
    }
    const fn visible(mut self, visibility: CommandVisibility) -> Self {
        self.visibility = visibility;
        self
    }
    const fn target(mut self, target: CommandTarget) -> Self {
        self.target = target;
        self
    }
    const fn runtime(mut self) -> Self {
        self.registrar = CommandRegistrar::Runtime;
        self
    }
}

use CommandSection::{
    ContextAndUsage, Conversation, ModelsAndAgents, Safety, SettingsAndHelp, Workspace,
};
use CommandTarget::{Engine, ScreenOrEngine};

/// Every built-in command, grouped by section in display order.
pub const COMMAND_CATALOG: &[CommandCatalogEntry] = &[
    entry(
        "new",
        "New session",
        Conversation,
        "Start a clean conversation",
    )
    .key("new_session"),
    entry(
        "resume",
        "Sessions",
        Conversation,
        "Resume, rename, or export a session",
    )
    .aliases(&["sessions"])
    .key("open_session_picker"),
    entry(
        "rewind",
        "Rewind",
        Conversation,
        "Restore or fork from a completed turn",
    )
    .arguments("[turn]")
    .availability(SessionActionKind::Rewind)
    .target(ScreenOrEngine),
    entry(
        "compact",
        "Compact",
        Conversation,
        "Summarize older context to free space",
    )
    .arguments("[instructions]")
    .availability(SessionActionKind::Compact)
    .target(Engine),
    entry(
        "queue",
        "Queued messages",
        Conversation,
        "Review, remove, or clear queued messages",
    )
    .visible(CommandVisibility::QueuedMessages),
    entry(
        "model",
        "Model",
        ModelsAndAgents,
        "Choose a model or connect a provider",
    )
    .aliases(&["models", "providers"])
    .key("open_model_picker")
    .availability(SessionActionKind::SwitchModel),
    entry(
        "mode",
        "Mode",
        ModelsAndAgents,
        "Switch between discuss, plan, and execute",
    )
    .arguments("[discuss|plan|execute]")
    .key("cycle_agent_mode")
    .availability(SessionActionKind::SwitchMode)
    .target(ScreenOrEngine),
    entry(
        "agents",
        "Agents",
        ModelsAndAgents,
        "Inspect, continue, or stop child agents",
    )
    .key("open_subagent_picker")
    .visible(CommandVisibility::ChildAgents),
    entry(
        "context",
        "Context",
        ContextAndUsage,
        "Inspect, pin, or evict context items",
    )
    .arguments("[pin|evict <item-id>]")
    .target(ScreenOrEngine),
    entry(
        "usage",
        "Usage",
        ContextAndUsage,
        "Tokens, cost, and budget limits",
    )
    .aliases(&["cost", "budget"]),
    entry(
        "review",
        "Review changes",
        Workspace,
        "Accept or revert this session's file changes",
    )
    .key("open_review")
    .availability(SessionActionKind::Review),
    entry(
        "dirs",
        "Directories",
        Workspace,
        "List workspace roots or add one",
    )
    .aliases(&["add-dir"])
    .arguments("[path]")
    .target(ScreenOrEngine),
    entry(
        "mcp",
        "MCP servers",
        Workspace,
        "Manage servers, prompts, and panels",
    )
    .arguments("[status|enable|disable|approve|prompt]")
    .target(ScreenOrEngine)
    .runtime(),
    entry(
        "init",
        "Init",
        Workspace,
        "Write AGENTS.md for this repository",
    )
    .arguments("[--deep]")
    .target(Engine)
    .runtime(),
    entry(
        "memory",
        "Memory",
        Workspace,
        "Read or update private project memory",
    )
    .arguments("[list|read <id>|write <text>|clear]")
    .target(Engine)
    .runtime(),
    entry(
        "permissions",
        "Permissions",
        Safety,
        "Approval policy, rules, and folder trust",
    )
    .aliases(&["trust"])
    .arguments("[mode|approvals|add|remove|trust|…]")
    .target(ScreenOrEngine),
    entry(
        "settings",
        "Settings",
        SettingsAndHelp,
        "Change saved user settings",
    ),
    entry(
        "skills",
        "Skills",
        SettingsAndHelp,
        "Skills, commands, and agents with their load status",
    ),
    entry(
        "theme",
        "Theme",
        SettingsAndHelp,
        "Preview and choose an interface theme",
    ),
    entry(
        "help",
        "Help",
        SettingsAndHelp,
        "Commands and keyboard shortcuts",
    )
    .aliases(&["keys"]),
    entry(
        "errors",
        "Errors",
        SettingsAndHelp,
        "Recent failures and recovery steps",
    )
    .visible(CommandVisibility::Errors),
    entry("exit", "Exit", SettingsAndHelp, "Close Rottweiler"),
];

/// Returns the entry with this canonical name.
#[must_use]
pub fn catalog_entry(name: &str) -> Option<&'static CommandCatalogEntry> {
    COMMAND_CATALOG.iter().find(|entry| entry.name == name)
}

/// Resolves a canonical name or alias to its entry.
#[must_use]
pub fn resolve_catalog_name(name: &str) -> Option<&'static CommandCatalogEntry> {
    COMMAND_CATALOG
        .iter()
        .find(|entry| entry.name == name || entry.aliases.contains(&name))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn names_and_aliases_are_unique_canonical_slash_names() {
        let mut seen = BTreeSet::new();
        for entry in COMMAND_CATALOG {
            for name in std::iter::once(&entry.name).chain(entry.aliases) {
                assert!(
                    !name.is_empty()
                        && name.bytes().all(|byte| byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || byte == b'-'),
                    "{name}"
                );
                assert!(seen.insert(*name), "duplicate command name {name}");
            }
            assert!(!entry.title.is_empty() && !entry.description.is_empty());
            assert!(!entry.description.ends_with('.'));
        }
    }

    #[test]
    fn entries_are_grouped_in_section_order() {
        let order = COMMAND_CATALOG
            .iter()
            .map(|entry| {
                COMMAND_SECTIONS
                    .iter()
                    .position(|section| *section == entry.section)
                    .expect("known section")
            })
            .collect::<Vec<_>>();
        assert!(order.windows(2).all(|pair| pair[0] <= pair[1]));
        for section in COMMAND_SECTIONS {
            assert!(
                COMMAND_CATALOG
                    .iter()
                    .any(|entry| entry.section == *section)
            );
        }
    }

    #[test]
    fn aliases_resolve_to_one_entry_and_screens_take_no_arguments() {
        assert_eq!(resolve_catalog_name("cost").map(|e| e.name), Some("usage"));
        assert_eq!(
            resolve_catalog_name("providers").map(|e| e.name),
            Some("model")
        );
        assert_eq!(
            resolve_catalog_name("trust").map(|e| e.name),
            Some("permissions")
        );
        assert_eq!(
            resolve_catalog_name("add-dir").map(|e| e.name),
            Some("dirs")
        );
        assert!(resolve_catalog_name("goto").is_none());
        for entry in COMMAND_CATALOG {
            if entry.target == CommandTarget::Screen {
                assert!(entry.argument_hint.is_empty(), "{}", entry.name);
            }
        }
        assert_eq!(
            catalog_entry("compact").map(CommandCatalogEntry::usage),
            Some("/compact [instructions]".to_owned())
        );
    }

    #[test]
    fn generated_projection_omits_host_registration() {
        let value = serde_json::to_value(catalog_entry("mcp").expect("mcp")).expect("json");
        assert_eq!(value["section"], "Workspace");
        assert_eq!(value["target"], "screen_or_engine");
        assert!(value.get("registrar").is_none());
        assert_eq!(value["availability"], serde_json::Value::Null);
    }
}
