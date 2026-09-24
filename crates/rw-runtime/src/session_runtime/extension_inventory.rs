//! Engine projection of declarative extension outcomes.
//!
//! One inventory answers "which skills, commands, and agents loaded, and why
//! did the others not": discovery refusals, precedence shadowing, untrusted
//! project artifacts, ignored `allowed-tools` entries, and slash names taken
//! by built-in commands. Doctor, logs, and clients read the same rows.

use super::allowed_tools::normalize_allowed_tools;
use rw_ext::{ArtifactKind, ArtifactOrigin, ArtifactScope, ExtensionCatalog};
use rw_tools::ToolRegistry;
use rw_types::{
    ExtensionArtifactKind, ExtensionArtifactScope, ExtensionArtifactStatus, ExtensionInventory,
    ExtensionInventoryEntry, MAX_EXTENSION_INVENTORY_ENTRIES,
};
use std::path::Path;

/// Matches the SKILL.md description limit, so a conforming description is
/// listed whole.
const MAX_INVENTORY_DESCRIPTION_CHARS: usize = 1024;

/// Builds the inventory for one discovery generation. `tools` enables
/// `allowed-tools` checks against the live registry; without it those notes
/// are omitted.
#[must_use]
pub fn extension_inventory(
    catalog: &ExtensionCatalog,
    tools: Option<&ToolRegistry>,
) -> ExtensionInventory {
    let mut entries = Vec::new();
    for skill in catalog.skills() {
        let mut notes = allowed_tool_notes(skill.allowed_tools(), tools);
        if let Some(note) = builtin_collision_note(skill.name()) {
            notes.push(note);
        }
        if catalog.command(skill.name()).is_some() {
            notes.push(format!(
                "slash command /{} runs the command of the same name; this skill stays available through the `skill` tool",
                skill.name()
            ));
        }
        entries.push(loaded(
            ExtensionArtifactKind::Skill,
            skill.name(),
            skill.description(),
            skill.origin(),
            notes,
        ));
    }
    for command in catalog.commands() {
        let mut notes = allowed_tool_notes(command.allowed_tools(), tools);
        if let Some(note) = builtin_collision_note(command.name()) {
            notes.push(note);
        }
        entries.push(loaded(
            ExtensionArtifactKind::Command,
            command.name(),
            command.description(),
            command.origin(),
            notes,
        ));
    }
    for agent in catalog.agents() {
        entries.push(loaded(
            ExtensionArtifactKind::Agent,
            agent.name(),
            agent.description(),
            agent.origin(),
            Vec::new(),
        ));
    }
    for diagnostic in catalog.diagnostics() {
        entries.push(ExtensionInventoryEntry {
            kind: artifact_kind(diagnostic.kind()),
            name: diagnostic.artifact_name().map(str::to_owned),
            description: String::new(),
            scope: artifact_scope(diagnostic.scope()),
            location: diagnostic.location().directory_name().to_owned(),
            source_path: display(diagnostic.path()),
            status: ExtensionArtifactStatus::Skipped,
            notes: vec![diagnostic.message().to_owned()],
        });
    }
    for shadowed in catalog.shadowed() {
        entries.push(ExtensionInventoryEntry {
            kind: artifact_kind(shadowed.kind()),
            name: Some(shadowed.name().to_owned()),
            description: String::new(),
            scope: artifact_scope(shadowed.origin().scope()),
            location: shadowed.origin().location().directory_name().to_owned(),
            source_path: display(shadowed.origin().path()),
            status: ExtensionArtifactStatus::Shadowed,
            notes: vec![format!(
                "hidden by the higher-precedence {} at {}",
                kind_label(shadowed.kind()),
                shadowed.selected_path().display()
            )],
        });
    }
    for artifact in catalog.inert_project_artifacts() {
        entries.push(ExtensionInventoryEntry {
            kind: artifact_kind(artifact.kind()),
            name: Some(artifact.name().to_owned()),
            description: String::new(),
            scope: ExtensionArtifactScope::Project,
            location: artifact.location().directory_name().to_owned(),
            source_path: display(artifact.path()),
            status: ExtensionArtifactStatus::Untrusted,
            notes: vec!["project is not trusted; trust the folder to load it".to_owned()],
        });
    }
    entries.sort_by(|left, right| {
        kind_order(left.kind)
            .cmp(&kind_order(right.kind))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.source_path.cmp(&right.source_path))
    });
    let truncated = entries.len() > MAX_EXTENSION_INVENTORY_ENTRIES;
    entries.truncate(MAX_EXTENSION_INVENTORY_ENTRIES);
    ExtensionInventory { entries, truncated }
}

/// Logs every non-loaded or annotated row once per discovery generation.
pub(super) fn log_extension_inventory(inventory: &ExtensionInventory) {
    for entry in &inventory.entries {
        if entry.status == ExtensionArtifactStatus::Loaded {
            continue;
        }
        tracing::info!(
            kind = ?entry.kind,
            name = entry.name.as_deref().unwrap_or(""),
            status = ?entry.status,
            path = entry.source_path,
            notes = entry.notes.join("; "),
            "declarative extension inventory note"
        );
    }
}

fn loaded(
    kind: ExtensionArtifactKind,
    name: &str,
    description: &str,
    origin: &ArtifactOrigin,
    notes: Vec<String>,
) -> ExtensionInventoryEntry {
    ExtensionInventoryEntry {
        kind,
        name: Some(name.to_owned()),
        description: inventory_description(description),
        scope: artifact_scope(origin.scope()),
        location: origin.location().directory_name().to_owned(),
        source_path: display(origin.path()),
        status: if notes.is_empty() {
            ExtensionArtifactStatus::Loaded
        } else {
            ExtensionArtifactStatus::LoadedWithWarnings
        },
        notes,
    }
}

/// Presentation text for a frontmatter description. YAML block scalars keep
/// the author's hard line breaks; single breaks are reflowed into spaces so
/// clients can wrap to their own width, and blank lines stay paragraph
/// breaks. An over-long description ends at a word boundary with `…`.
fn inventory_description(raw: &str) -> String {
    let mut paragraphs = Vec::new();
    let mut current = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join(" "));
                current.clear();
            }
        } else {
            current.extend(line.split_whitespace());
        }
    }
    if !current.is_empty() {
        paragraphs.push(current.join(" "));
    }
    let text = paragraphs.join("\n\n");
    if text.chars().count() <= MAX_INVENTORY_DESCRIPTION_CHARS {
        return text;
    }
    let limit = text
        .char_indices()
        .nth(MAX_INVENTORY_DESCRIPTION_CHARS - 1)
        .map_or(text.len(), |(index, _)| index);
    let cut = text[..limit]
        .rfind(char::is_whitespace)
        .filter(|&boundary| boundary > 0)
        .unwrap_or(limit);
    format!("{}…", text[..cut].trim_end())
}

fn allowed_tool_notes(configured: &[String], tools: Option<&ToolRegistry>) -> Vec<String> {
    tools.map_or_else(Vec::new, |tools| {
        normalize_allowed_tools(configured, tools).ignored
    })
}

/// Slash names owned by the engine command catalog cannot be replaced by an
/// extension; the artifact stays listed and, for skills, model-invocable.
pub(super) fn builtin_collision_note(name: &str) -> Option<String> {
    rw_types::client_navigation::resolve_catalog_name(name).map(|entry| {
        format!(
            "slash command /{name} is the built-in /{} command; this artifact is not reachable by that slash name",
            entry.name
        )
    })
}

const fn artifact_kind(kind: ArtifactKind) -> ExtensionArtifactKind {
    match kind {
        ArtifactKind::Skill => ExtensionArtifactKind::Skill,
        ArtifactKind::Command => ExtensionArtifactKind::Command,
        ArtifactKind::Agent => ExtensionArtifactKind::Agent,
        ArtifactKind::Workflow => ExtensionArtifactKind::Workflow,
        ArtifactKind::Mode => ExtensionArtifactKind::Mode,
        ArtifactKind::Hook => ExtensionArtifactKind::Hook,
    }
}

const fn kind_label(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Skill => "skill",
        ArtifactKind::Command => "command",
        ArtifactKind::Agent => "agent",
        ArtifactKind::Workflow => "workflow",
        ArtifactKind::Mode => "mode",
        ArtifactKind::Hook => "hook",
    }
}

const fn kind_order(kind: ExtensionArtifactKind) -> u8 {
    match kind {
        ExtensionArtifactKind::Skill => 0,
        ExtensionArtifactKind::Command => 1,
        ExtensionArtifactKind::Agent => 2,
        ExtensionArtifactKind::Workflow => 3,
        ExtensionArtifactKind::Mode => 4,
        ExtensionArtifactKind::Hook => 5,
    }
}

const fn artifact_scope(scope: ArtifactScope) -> ExtensionArtifactScope {
    match scope {
        ArtifactScope::Project => ExtensionArtifactScope::Project,
        ArtifactScope::User => ExtensionArtifactScope::User,
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::{MAX_INVENTORY_DESCRIPTION_CHARS, inventory_description};

    #[test]
    fn block_scalar_descriptions_reflow_and_keep_paragraphs() {
        let raw = "Use when the user\n  wants to run the formatter\nbefore committing.\n\nNever edits\ngenerated files.\n";
        assert_eq!(
            inventory_description(raw),
            "Use when the user wants to run the formatter before committing.\n\nNever edits generated files."
        );
    }

    #[test]
    fn long_descriptions_end_at_a_word_boundary() {
        let raw = "formatter ".repeat(200);
        let shown = inventory_description(&raw);
        assert!(shown.chars().count() <= MAX_INVENTORY_DESCRIPTION_CHARS);
        assert!(shown.ends_with("formatter…"), "{shown}");
        let short = "a".repeat(MAX_INVENTORY_DESCRIPTION_CHARS);
        assert_eq!(inventory_description(&short), short);
    }
}
