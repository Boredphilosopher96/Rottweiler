use super::{
    MAX_UI_ACTION_ARGUMENT_BYTES, MAX_UI_ACTIONS, MAX_UI_CONTRIBUTIONS, MAX_UI_DESCRIPTOR_BYTES,
    MAX_UI_FIELDS, MAX_UI_LABEL_BYTES, MAX_UI_LIST_ITEMS, MAX_UI_SELECTOR_STEPS,
    MAX_UI_SURFACE_BYTES, MAX_UI_TABLE_COLUMNS, MAX_UI_TABLE_ROWS, MAX_UI_VALUE_BYTES,
    UiContractError, UiContribution, UiField, UiProjectedField, UiProjectedFields, UiSelectorStep,
};
use serde::Serialize;
use std::collections::BTreeSet;

/// # Errors
/// Rejects duplicate identities, unbounded selectors, display collections and
/// executable-shaped identifiers before a manifest can be approved.
pub fn validate_contributions(items: &[UiContribution]) -> Result<(), UiContractError> {
    if items.len() > MAX_UI_CONTRIBUTIONS {
        return Err(UiContractError("descriptor count"));
    }
    encoded_bytes(items, MAX_UI_DESCRIPTOR_BYTES)?;
    let mut ids = BTreeSet::new();
    let mut tools = BTreeSet::new();
    for item in items {
        identifier(item.id())?;
        text(item.title(), MAX_UI_LABEL_BYTES)?;
        if !ids.insert(item.id()) {
            return Err(UiContractError("duplicate contribution"));
        }
        if let UiContribution::Tool { tool_name, .. } = item {
            identifier(tool_name)?;
            if !tools.insert(tool_name) {
                return Err(UiContractError("duplicate tool presenter"));
            }
        }
        validate_fields(item.fields())?;
        if item.actions().len() > MAX_UI_ACTIONS {
            return Err(UiContractError("action count"));
        }
        let mut actions = BTreeSet::new();
        for action in item.actions() {
            identifier(&action.id)?;
            identifier(&action.command)?;
            text(&action.label, MAX_UI_LABEL_BYTES)?;
            encoded_bytes(&action.arguments, MAX_UI_ACTION_ARGUMENT_BYTES)?;
            if !actions.insert(&action.id) {
                return Err(UiContractError("duplicate action"));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_fields(fields: &[UiField]) -> Result<(), UiContractError> {
    if fields.len() > MAX_UI_FIELDS {
        return Err(UiContractError("field count"));
    }
    let mut ids = BTreeSet::new();
    for field in fields {
        identifier(field.id())?;
        text(field.label(), MAX_UI_LABEL_BYTES)?;
        selector(field.path())?;
        if !ids.insert(field.id()) {
            return Err(UiContractError("duplicate field"));
        }
        match field {
            UiField::List { max_items, .. }
                if *max_items == 0 || u64::from(*max_items) > MAX_UI_LIST_ITEMS as u64 =>
            {
                return Err(UiContractError("list limit"));
            }
            UiField::Table {
                columns, max_rows, ..
            } => {
                if *max_rows == 0
                    || u64::from(*max_rows) > MAX_UI_TABLE_ROWS as u64
                    || columns.is_empty()
                    || columns.len() > MAX_UI_TABLE_COLUMNS
                {
                    return Err(UiContractError("table limit"));
                }
                for column in columns {
                    text(&column.label, MAX_UI_LABEL_BYTES)?;
                    selector(&column.path)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn selector(path: &[UiSelectorStep]) -> Result<(), UiContractError> {
    if path.len() > MAX_UI_SELECTOR_STEPS {
        return Err(UiContractError("selector depth"));
    }
    for step in path {
        if let UiSelectorStep::Field { name } = step {
            text(name, MAX_UI_LABEL_BYTES)?;
        }
    }
    Ok(())
}

fn identifier(value: &str) -> Result<(), UiContractError> {
    if value.is_empty()
        || value.len() > MAX_UI_LABEL_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(UiContractError("identifier"));
    }
    Ok(())
}

pub(super) fn text(value: &str, limit: usize) -> Result<(), UiContractError> {
    if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
        return Err(UiContractError("text bound"));
    }
    Ok(())
}

/// # Errors
/// Rejects wrong field identities/kinds, undeclared collection sizes and byte bounds.
pub fn validate_projected_fields(
    declared: &[UiField],
    projected: &UiProjectedFields,
) -> Result<(), UiContractError> {
    validate_fields(declared)?;
    encoded_bytes(projected, MAX_UI_SURFACE_BYTES)?;
    if declared.len() != projected.fields.len() {
        return Err(UiContractError("projected field count"));
    }
    for (field, projected) in declared.iter().zip(&projected.fields) {
        let valid = match (field, projected) {
            (UiField::Text { id, .. }, UiProjectedField::Text { id: actual, value })
            | (UiField::Badge { id, .. }, UiProjectedField::Badge { id: actual, value }) => {
                id == actual && value.as_ref().is_none_or(|value| display_text(value))
            }
            (
                UiField::List { id, max_items, .. },
                UiProjectedField::List { id: actual, values },
            ) => {
                id == actual
                    && values.len() as u64 <= u64::from(*max_items)
                    && values.iter().all(|value| display_text(value))
            }
            (
                UiField::Table {
                    id,
                    columns,
                    max_rows,
                    ..
                },
                UiProjectedField::Table { id: actual, rows },
            ) => {
                id == actual
                    && rows.len() as u64 <= u64::from(*max_rows)
                    && rows.iter().all(|row| {
                        row.len() == columns.len() && row.iter().all(|value| display_text(value))
                    })
            }
            _ => false,
        };
        if !valid {
            return Err(UiContractError("projected field contract"));
        }
    }
    Ok(())
}

fn display_text(value: &str) -> bool {
    value.len() <= MAX_UI_VALUE_BYTES
        && !value
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
}

pub(super) fn encoded_bytes<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
) -> Result<usize, UiContractError> {
    struct Counter {
        count: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.count = self
                .count
                .checked_add(bytes.len())
                .filter(|count| *count <= self.limit)
                .ok_or_else(|| std::io::Error::other("presentation byte budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { count: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| UiContractError("serialized byte budget"))?;
    Ok(counter.count)
}
