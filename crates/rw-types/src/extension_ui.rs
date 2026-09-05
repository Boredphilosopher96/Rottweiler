//! Declarative presentation data. The host evaluates selectors; clients render
//! bounded projected fields without executing extension code.
mod client;
pub use client::{
    UiActionRequest, UiActionTarget, UiCatalog, UiCatalogEntry, UiContributionOwner,
    UiDisplayAction, UiDisplayDescriptor, UiDisplayField, UiDisplaySurface, UiGenerationId,
    UiPanelSnapshot, UiPresentation,
};
mod wire_bounds;
use wire_bounds::{
    bounded_column_labels, bounded_display_list, bounded_display_table, nullable_display_value,
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
    Field {
        #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
        name: String,
    },
    Index {
        index: u32,
    },
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiTableColumn {
    #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
    pub label: String,
    #[schemars(length(max = 16))]
    pub path: Vec<UiSelectorStep>,
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiField {
    Text {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
        label: String,
        #[schemars(length(max = 16))]
        path: Vec<UiSelectorStep>,
    },
    Badge {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
        label: String,
        #[schemars(length(max = 16))]
        path: Vec<UiSelectorStep>,
    },
    List {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
        label: String,
        #[schemars(length(max = 16))]
        path: Vec<UiSelectorStep>,
        #[schemars(range(min = 1, max = 32))]
        max_items: u32,
    },
    Table {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
        label: String,
        #[schemars(length(max = 16))]
        path: Vec<UiSelectorStep>,
        #[schemars(length(min = 1, max = 8))]
        columns: Vec<UiTableColumn>,
        #[schemars(range(min = 1, max = 32))]
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
    #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
    pub id: String,
    #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
    pub label: String,
    #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
    pub command: String,
    #[schemars(extend("x-rw-max-json-bytes"=4096))]
    pub arguments: Value,
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiContribution {
    Tool {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        tool_name: String,
        #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
        title: String,
        #[schemars(length(max = 32))]
        fields: Vec<UiField>,
        #[schemars(length(max = 4))]
        actions: Vec<UiAction>,
    },
    Panel {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),regex(pattern=r"^[^\u0000-\u001f\u007f-\u009f]+$"),extend("x-rw-max-utf8-bytes"=128))]
        title: String,
        #[schemars(length(max = 32))]
        fields: Vec<UiField>,
        #[schemars(length(max = 4))]
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
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "nullable_display_value")]
        value: Option<String>,
    },
    Badge {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "nullable_display_value")]
        value: Option<String>,
    },
    List {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(schema_with = "bounded_display_list")]
        values: Vec<String>,
    },
    Table {
        #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(schema_with = "bounded_display_table")]
        rows: Vec<Vec<String>>,
    },
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiProjectedFields {
    #[schemars(length(max = 32))]
    pub fields: Vec<UiProjectedField>,
    /// At least one source value was truncated by a declared or aggregate bound.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid extension presentation: {0}")]
pub struct UiContractError(pub &'static str);

#[cfg(test)]
mod tests;
