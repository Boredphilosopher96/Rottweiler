//! Client-facing presentation values contain neither selectors nor executable action data.
mod admission;
use super::{
    MAX_UI_DESCRIPTOR_BYTES, MAX_UI_SURFACE_BYTES, UiContractError, UiContribution, UiField,
    UiProjectedFields, projection, validate_contributions, validation,
};
use crate::ToolInvocationId;
use rw_memory_derive::PrepareAllocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// An application-minted endpoint epoch; restarting the same artifact creates a
/// different generation. It is a correlation identifier, never permission.
#[derive(
    Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(transparent)]
pub struct UiGenerationId(
    #[schemars(length(min = 32, max = 32), regex(pattern = "^[0-9a-f]{32}$"))] String,
);
impl UiGenerationId {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(32);
        for byte in bytes {
            text.push(char::from(HEX[usize::from(byte >> 4)]));
            text.push(char::from(HEX[usize::from(byte & 15)]));
        }
        Self(text)
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for UiGenerationId {
    type Error = UiContractError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(UiContractError("generation identity"));
        }
        Ok(Self(value))
    }
}
impl<'de> Deserialize<'de> for UiGenerationId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiContributionOwner {
    #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
    pub extension: String,
    pub generation: UiGenerationId,
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiDisplayField {
    Text {
        #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
        label: String,
    },
    Badge {
        #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
        label: String,
    },
    List {
        #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
        label: String,
        #[schemars(range(min = 1, max = 32))]
        max_items: u32,
    },
    Table {
        #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
        id: String,
        #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
        label: String,
        #[schemars(schema_with = "super::bounded_column_labels")]
        columns: Vec<String>,
        #[schemars(range(min = 1, max = 32))]
        max_rows: u32,
    },
}
#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiDisplayAction {
    #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
    pub id: String,
    #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
    pub label: String,
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiDisplaySurface {
    Tool { tool_name: String },
    Panel {},
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiDisplayDescriptor {
    #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
    pub id: String,
    #[schemars(length(min=1,max=128),extend("x-rw-max-utf8-bytes"=128))]
    pub title: String,
    pub surface: UiDisplaySurface,
    #[schemars(length(max = 32))]
    pub fields: Vec<UiDisplayField>,
    #[schemars(length(max = 4))]
    pub actions: Vec<UiDisplayAction>,
}
impl UiDisplayDescriptor {
    /// # Errors
    /// Rejects an invalid declaration before projecting its display-only shape.
    pub fn from_declaration(declaration: &UiContribution) -> Result<Self, UiContractError> {
        validate_contributions(std::slice::from_ref(declaration))?;
        Ok(Self {
            id: declaration.id().into(),
            title: declaration.title().into(),
            surface: match declaration {
                UiContribution::Tool { tool_name, .. } => UiDisplaySurface::Tool {
                    tool_name: tool_name.clone(),
                },
                UiContribution::Panel { .. } => UiDisplaySurface::Panel {},
            },
            fields: declaration
                .fields()
                .iter()
                .map(|field| match field {
                    UiField::Text { id, label, .. } => UiDisplayField::Text {
                        id: id.clone(),
                        label: label.clone(),
                    },
                    UiField::Badge { id, label, .. } => UiDisplayField::Badge {
                        id: id.clone(),
                        label: label.clone(),
                    },
                    UiField::List {
                        id,
                        label,
                        max_items,
                        ..
                    } => UiDisplayField::List {
                        id: id.clone(),
                        label: label.clone(),
                        max_items: *max_items,
                    },
                    UiField::Table {
                        id,
                        label,
                        columns,
                        max_rows,
                        ..
                    } => UiDisplayField::Table {
                        id: id.clone(),
                        label: label.clone(),
                        columns: columns.iter().map(|column| column.label.clone()).collect(),
                        max_rows: *max_rows,
                    },
                })
                .collect(),
            actions: declaration
                .actions()
                .iter()
                .map(|action| UiDisplayAction {
                    id: action.id.clone(),
                    label: action.label.clone(),
                })
                .collect(),
        })
    }
}

/// A self-contained, host-produced surface. A canonical tool result retains this
/// descriptor so historical rendering does not consult a different live plugin.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation)]
#[schemars(extend("x-rw-max-json-bytes" = MAX_UI_SURFACE_BYTES))]
#[serde(deny_unknown_fields)]
pub struct UiPresentation {
    pub owner: UiContributionOwner,
    pub descriptor: UiDisplayDescriptor,
    pub projected: UiProjectedFields,
}
impl UiPresentation {
    /// # Errors
    /// Rejects invalid declarations/identity. Values are truncated within the
    /// complete encoded surface allowance before being retained.
    pub fn project(
        owner: UiContributionOwner,
        declaration: &UiContribution,
        data: &serde_json::Value,
    ) -> Result<Self, UiContractError> {
        validation::identifier(&owner.extension)?;
        let descriptor = UiDisplayDescriptor::from_declaration(declaration)?;
        let overhead = validation::encoded_bytes(&(&owner, &descriptor), MAX_UI_DESCRIPTOR_BYTES)?;
        let limit = MAX_UI_SURFACE_BYTES
            .checked_sub(overhead + 1024)
            .ok_or(UiContractError("display descriptor surface budget"))?;
        let projected = projection::project_fields_with_budget(declaration.fields(), data, limit)?;
        let surface = Self {
            owner,
            descriptor,
            projected,
        };
        validation::encoded_bytes(&surface, MAX_UI_SURFACE_BYTES)?;
        Ok(surface)
    }
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiCatalogEntry {
    pub owner: UiContributionOwner,
    pub descriptor: UiDisplayDescriptor,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation)]
#[schemars(extend("x-rw-max-json-bytes" = MAX_UI_DESCRIPTOR_BYTES))]
#[serde(deny_unknown_fields)]
pub struct UiCatalog {
    #[schemars(length(max = 128))]
    pub entries: Vec<UiCatalogEntry>,
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiActionTarget {
    Tool { invocation_id: ToolInvocationId },
    Panel { revision: u32 },
}

/// Action arguments and command selection remain in the approved host registry.
#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiActionRequest {
    pub owner: UiContributionOwner,
    pub contribution_id: String,
    pub action_id: String,
    pub target: UiActionTarget,
}

#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiPanelSnapshot {
    pub revision: u32,
    pub presentation: UiPresentation,
}

#[cfg(test)]
mod tests;

/// One coalesced surface per live panel. Historical tool surfaces use their
/// canonical result source and do not consume this ephemeral registry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-rw-max-json-bytes"=524_288))]
pub struct UiPanels {
    #[schemars(length(max = 8))]
    pub panels: Vec<UiPanelSnapshot>,
}

/// Panel publication carries only source data; the host owns selectors,
/// generation identity and the monotonically increasing revision.
#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiPanelUpdate {
    #[schemars(length(min=1,max=128),regex(pattern="^[A-Za-z0-9_.-]+$"),extend("x-rw-max-utf8-bytes"=128))]
    pub id: String,
    #[schemars(extend("x-rw-max-json-bytes"=65536))]
    pub data: serde_json::Value,
}
impl UiPanelUpdate {
    /// # Errors
    /// Rejects invalid names and source data outside the publication allowance.
    pub fn validate(&self) -> Result<(), UiContractError> {
        validation::identifier(&self.id)?;
        validation::encoded_bytes(&self.data, 64 * 1024)?;
        Ok(())
    }
}
#[derive(
    Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation,
)]
#[serde(deny_unknown_fields)]
pub struct UiPanelUpdated {
    #[schemars(range(min = 1))]
    pub revision: u32,
}
