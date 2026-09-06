use super::{
    UiCatalog, UiCatalogEntry, UiContributionOwner, UiDisplayDescriptor, UiDisplayField,
    UiDisplaySurface, UiPresentation,
};
use crate::extension_ui::{
    MAX_UI_CONTRIBUTIONS, MAX_UI_DESCRIPTOR_BYTES, MAX_UI_PANEL_SLOTS, MAX_UI_PANELS_BYTES,
    MAX_UI_SURFACE_BYTES, UiContractError, UiField, UiProjectedFields, UiTableColumn,
    validate_projected_fields, validation,
};
use serde::Deserialize;
use std::collections::BTreeSet;

impl UiDisplayDescriptor {
    pub(super) fn projection_fields(&self) -> Vec<UiField> {
        self.fields
            .iter()
            .map(|field| match field {
                UiDisplayField::Text { id, label } => UiField::Text {
                    id: id.clone(),
                    label: label.clone(),
                    path: Vec::new(),
                },
                UiDisplayField::Badge { id, label } => UiField::Badge {
                    id: id.clone(),
                    label: label.clone(),
                    path: Vec::new(),
                },
                UiDisplayField::List {
                    id,
                    label,
                    max_items,
                } => UiField::List {
                    id: id.clone(),
                    label: label.clone(),
                    path: Vec::new(),
                    max_items: *max_items,
                },
                UiDisplayField::Table {
                    id,
                    label,
                    columns,
                    max_rows,
                } => UiField::Table {
                    id: id.clone(),
                    label: label.clone(),
                    path: Vec::new(),
                    columns: columns
                        .iter()
                        .map(|label| UiTableColumn {
                            label: label.clone(),
                            path: Vec::new(),
                        })
                        .collect(),
                    max_rows: *max_rows,
                },
            })
            .collect()
    }
    fn validate(&self) -> Result<(), UiContractError> {
        validation::encoded_bytes(self, MAX_UI_SURFACE_BYTES)?;
        validation::identifier(&self.id)?;
        validation::text(&self.title, 128)?;
        if let UiDisplaySurface::Tool { tool_name } = &self.surface {
            validation::identifier(tool_name)?;
        }
        validation::validate_fields(&self.projection_fields())?;
        if self.actions.len() > 4 {
            return Err(UiContractError("display action count"));
        }
        let mut ids = BTreeSet::new();
        for action in &self.actions {
            validation::identifier(&action.id)?;
            validation::text(&action.label, 128)?;
            if !ids.insert(&action.id) {
                return Err(UiContractError("duplicate display action"));
            }
        }
        Ok(())
    }
}
impl UiPresentation {
    /// # Errors
    /// Rejects malformed decoded presentation bounds before client retention.
    pub fn validate(&self) -> Result<(), UiContractError> {
        validation::encoded_bytes(self, MAX_UI_SURFACE_BYTES)?;
        validation::identifier(&self.owner.extension)?;
        self.descriptor.validate()?;
        validate_projected_fields(&self.descriptor.projection_fields(), &self.projected)
    }
}
impl<'de> Deserialize<'de> for UiPresentation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            owner: UiContributionOwner,
            descriptor: UiDisplayDescriptor,
            projected: UiProjectedFields,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            owner: wire.owner,
            descriptor: wire.descriptor,
            projected: wire.projected,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}
impl UiCatalog {
    /// # Errors
    /// Rejects aggregate descriptor bytes/counts and duplicate generation identities.
    pub fn validate(&self) -> Result<(), UiContractError> {
        validation::encoded_bytes(self, MAX_UI_DESCRIPTOR_BYTES)?;
        if self.entries.len() > MAX_UI_CONTRIBUTIONS {
            return Err(UiContractError("catalog descriptor count"));
        }
        let mut identities = BTreeSet::new();
        for entry in &self.entries {
            validation::identifier(&entry.owner.extension)?;
            entry.descriptor.validate()?;
            if !identities.insert((&entry.owner.generation, &entry.descriptor.id)) {
                return Err(UiContractError("duplicate catalog descriptor"));
            }
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for UiCatalog {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            entries: Vec<UiCatalogEntry>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            entries: wire.entries,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl super::UiPanels {
    /// # Errors
    /// Rejects repeated panels, excess count/encoded bytes or invalid surfaces.
    pub fn validate(&self) -> Result<(), UiContractError> {
        validation::encoded_bytes(self, MAX_UI_PANELS_BYTES)?;
        if self.panels.len() > MAX_UI_PANEL_SLOTS {
            return Err(UiContractError("panel count"));
        }
        let mut identities = BTreeSet::new();
        for panel in &self.panels {
            panel.presentation.validate()?;
            if panel.revision == 0
                || !matches!(
                    panel.presentation.descriptor.surface,
                    UiDisplaySurface::Panel {}
                )
            {
                return Err(UiContractError("panel surface identity"));
            }
            if !identities.insert((
                &panel.presentation.owner.generation,
                &panel.presentation.descriptor.id,
            )) {
                return Err(UiContractError("duplicate panel identity"));
            }
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for super::UiPanels {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            panels: Vec<super::UiPanelSnapshot>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            panels: wire.panels,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}
