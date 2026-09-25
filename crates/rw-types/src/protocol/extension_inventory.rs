//! Declarative extension inventory: which skills, commands, and agents a
//! session loaded, and why others were skipped or hidden.
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Most entries carried by one inventory projection.
pub const MAX_EXTENSION_INVENTORY_ENTRIES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ExtensionArtifactKind {
    Skill,
    Command,
    Agent,
    Workflow,
    Mode,
    Hook,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ExtensionArtifactScope {
    Project,
    User,
}

/// Outcome of one discovered artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ExtensionArtifactStatus {
    /// Active and invocable.
    Loaded,
    /// Active, with a non-fatal note (for example ignored `allowed-tools`
    /// entries, or a slash name taken by a built-in command).
    LoadedWithWarnings,
    /// Not loaded; `reason` says why.
    Skipped,
    /// Valid but hidden by a higher-precedence artifact of the same name.
    Shadowed,
    /// Project artifact held inactive until the folder is trusted.
    Untrusted,
}

/// One row of the extension inventory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInventoryEntry {
    pub kind: ExtensionArtifactKind,
    /// Artifact name when one could be determined.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    pub name: Option<String>,
    /// Description for loaded artifacts; empty otherwise.
    pub description: String,
    pub scope: ExtensionArtifactScope,
    /// `.agents`, `.rottweiler`, or `.claude`.
    pub location: String,
    /// Source file as discovered.
    pub source_path: String,
    pub status: ExtensionArtifactStatus,
    /// Human-readable explanation for every status other than `loaded`.
    pub notes: Vec<String>,
}

/// Bounded inventory of declarative extensions for one session.
#[derive(
    Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation,
)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInventory {
    #[schemars(length(max = 512))]
    pub entries: Vec<ExtensionInventoryEntry>,
    /// More entries existed than [`MAX_EXTENSION_INVENTORY_ENTRIES`].
    pub truncated: bool,
}
