//! Typed selector construction for source-owned display declarations.
use rw_types::extension_ui::{UiField, UiSelectorStep, UiTableColumn};

fn path(fields: &[&str]) -> Vec<UiSelectorStep> {
    fields
        .iter()
        .map(|name| UiSelectorStep::Field {
            name: (*name).into(),
        })
        .collect()
}
#[must_use]
pub fn text(id: &str, label: &str, fields: &[&str]) -> UiField {
    UiField::Text {
        id: id.into(),
        label: label.into(),
        path: path(fields),
    }
}
#[must_use]
pub fn badge(id: &str, label: &str, fields: &[&str]) -> UiField {
    UiField::Badge {
        id: id.into(),
        label: label.into(),
        path: path(fields),
    }
}
#[must_use]
pub fn list(id: &str, label: &str, fields: &[&str]) -> UiField {
    UiField::List {
        id: id.into(),
        label: label.into(),
        path: path(fields),
        max_items: 8,
    }
}
#[must_use]
pub fn table(id: &str, label: &str, fields: &[&str], columns: &[(&str, &[&str])]) -> UiField {
    UiField::Table {
        id: id.into(),
        label: label.into(),
        path: path(fields),
        max_rows: 8,
        columns: columns
            .iter()
            .map(|(label, fields)| UiTableColumn {
                label: (*label).into(),
                path: path(fields),
            })
            .collect(),
    }
}
