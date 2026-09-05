//! Declarative presentation data. The host evaluates selectors; clients render
//! bounded projected fields without executing extension code.
mod client;
pub use client::{
    UiActionRequest, UiActionTarget, UiCatalog, UiCatalogEntry, UiContributionOwner,
    UiDisplayAction, UiDisplayDescriptor, UiDisplayField, UiDisplaySurface, UiGenerationId,
    UiPanelSnapshot, UiPresentation,
};
mod projection;
mod validation;
pub use projection::project_fields;
pub use validation::{validate_contributions, validate_projected_fields};

use rw_memory_derive::PrepareAllocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

pub const MAX_UI_CONTRIBUTIONS: usize = 128;
pub const MAX_UI_DESCRIPTOR_BYTES: usize = 256 * 1024;
pub const MAX_UI_FIELDS: usize = 32;
pub const MAX_UI_SELECTOR_STEPS: usize = 16;
pub const MAX_UI_ACTIONS: usize = 4;
pub const MAX_UI_LABEL_BYTES: usize = 128;
pub const MAX_UI_VALUE_BYTES: usize = 4096;
pub const MAX_UI_SURFACE_BYTES: usize = 64 * 1024;
pub const MAX_UI_LIST_ITEMS: usize = 32;
pub const MAX_UI_TABLE_ROWS: usize = 32;
pub const MAX_UI_TABLE_COLUMNS: usize = 8;
pub const MAX_UI_ACTION_ARGUMENT_BYTES: usize = 4096;

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiSelectorStep {
    Field { name: String },
    Index { index: u32 },
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiTableColumn {
    pub label: String,
    pub path: Vec<UiSelectorStep>,
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiField {
    Text {
        id: String,
        label: String,
        path: Vec<UiSelectorStep>,
    },
    Badge {
        id: String,
        label: String,
        path: Vec<UiSelectorStep>,
    },
    List {
        id: String,
        label: String,
        path: Vec<UiSelectorStep>,
        max_items: u32,
    },
    Table {
        id: String,
        label: String,
        path: Vec<UiSelectorStep>,
        columns: Vec<UiTableColumn>,
        max_rows: u32,
    },
}

impl UiField {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. }
            | Self::Badge { id, .. }
            | Self::List { id, .. }
            | Self::Table { id, .. } => id,
        }
    }
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Text { label, .. }
            | Self::Badge { label, .. }
            | Self::List { label, .. }
            | Self::Table { label, .. } => label,
        }
    }
    #[must_use]
    pub fn path(&self) -> &[UiSelectorStep] {
        match self {
            Self::Text { path, .. }
            | Self::Badge { path, .. }
            | Self::List { path, .. }
            | Self::Table { path, .. } => path,
        }
    }
}

/// A command identifier, never a shell fragment, file path, URL or executable.
#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiAction {
    pub id: String,
    pub label: String,
    pub command: String,
    pub arguments: Value,
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiContribution {
    Tool {
        id: String,
        tool_name: String,
        title: String,
        fields: Vec<UiField>,
        actions: Vec<UiAction>,
    },
    Panel {
        id: String,
        title: String,
        fields: Vec<UiField>,
        actions: Vec<UiAction>,
    },
}

impl UiContribution {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Tool { id, .. } | Self::Panel { id, .. } => id,
        }
    }
    #[must_use]
    pub fn fields(&self) -> &[UiField] {
        match self {
            Self::Tool { fields, .. } | Self::Panel { fields, .. } => fields,
        }
    }
    #[must_use]
    pub fn actions(&self) -> &[UiAction] {
        match self {
            Self::Tool { actions, .. } | Self::Panel { actions, .. } => actions,
        }
    }
    #[must_use]
    pub fn title(&self) -> &str {
        match self {
            Self::Tool { title, .. } | Self::Panel { title, .. } => title,
        }
    }
}

/// Host-produced data, keyed by the declared field identity. Missing/wrong-shaped
/// source values become null or empty collections instead of executing coercions.
#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiProjectedField {
    Text {
        id: String,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
        value: Option<String>,
    },
    Badge {
        id: String,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
        value: Option<String>,
    },
    List {
        id: String,
        values: Vec<String>,
    },
    Table {
        id: String,
        rows: Vec<Vec<String>>,
    },
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiProjectedFields {
    pub fields: Vec<UiProjectedField>,
    /// At least one source value was truncated by a declared or aggregate bound.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid extension presentation: {0}")]
pub struct UiContractError(pub &'static str);

#[cfg(test)]
mod tests;
