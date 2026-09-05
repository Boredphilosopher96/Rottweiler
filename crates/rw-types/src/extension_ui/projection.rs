use super::*;

/// # Errors
/// Rejects an invalid descriptor. Projection visits at most the declared field,
/// selector, row and item bounds; it never stringifies an unbounded source object.
pub fn project_fields(
    fields: &[UiField],
    source: &Value,
) -> Result<UiProjectedFields, UiContractError> {
    validation::validate_fields(fields)?;
    let mut output = UiProjectedFields {
        fields: fields.iter().map(empty).collect(),
        truncated: false,
    };
    let base = validation::encoded_bytes(&output, MAX_UI_SURFACE_BYTES)?;
    // Account for commas/quotes before retaining values, including JSON escaping.
    let mut budget = Budget {
        bytes: MAX_UI_SURFACE_BYTES - base,
        truncated: false,
    };
    for (field, projected) in fields.iter().zip(&mut output.fields) {
        let selected = select(source, field.path());
        match (field, projected) {
            (UiField::Text { .. }, UiProjectedField::Text { value, .. })
            | (UiField::Badge { .. }, UiProjectedField::Badge { value, .. }) => {
                if let Some(text) = selected.and_then(Value::as_str)
                    && budget.reserve(2)
                {
                    *value = Some(budget.text(text));
                }
            }
            (UiField::List { max_items, .. }, UiProjectedField::List { values, .. }) => {
                if let Some(items) = selected.and_then(Value::as_array) {
                    let limit =
                        usize::try_from(*max_items).map_err(|_| UiContractError("list limit"))?;
                    budget.truncated |= items.len() > limit;
                    for item in items.iter().take(limit) {
                        if !budget.reserve(3) {
                            break;
                        }
                        values.push(budget.text(item.as_str().unwrap_or("")));
                    }
                }
            }
            (
                UiField::Table {
                    columns, max_rows, ..
                },
                UiProjectedField::Table { rows, .. },
            ) => {
                if let Some(items) = selected.and_then(Value::as_array) {
                    let limit =
                        usize::try_from(*max_rows).map_err(|_| UiContractError("table limit"))?;
                    budget.truncated |= items.len() > limit;
                    for item in items.iter().take(limit) {
                        if !budget.reserve(3 + columns.len() * 3) {
                            break;
                        }
                        rows.push(
                            columns
                                .iter()
                                .map(|column| {
                                    budget.text(
                                        select(item, &column.path)
                                            .and_then(Value::as_str)
                                            .unwrap_or(""),
                                    )
                                })
                                .collect(),
                        );
                    }
                }
            }
            _ => return Err(UiContractError("projection kind")),
        }
    }
    output.truncated = budget.truncated;
    validate_projected_fields(fields, &output)?;
    Ok(output)
}

fn empty(field: &UiField) -> UiProjectedField {
    let id = field.id().to_owned();
    match field {
        UiField::Text { .. } => UiProjectedField::Text { id, value: None },
        UiField::Badge { .. } => UiProjectedField::Badge { id, value: None },
        UiField::List { .. } => UiProjectedField::List {
            id,
            values: Vec::new(),
        },
        UiField::Table { .. } => UiProjectedField::Table {
            id,
            rows: Vec::new(),
        },
    }
}

fn select<'a>(mut value: &'a Value, path: &[UiSelectorStep]) -> Option<&'a Value> {
    for step in path {
        value = match step {
            UiSelectorStep::Field { name } => value.as_object()?.get(name)?,
            UiSelectorStep::Index { index } => {
                value.as_array()?.get(usize::try_from(*index).ok()?)?
            }
        };
    }
    Some(value)
}

struct Budget {
    bytes: usize,
    truncated: bool,
}
impl Budget {
    fn reserve(&mut self, bytes: usize) -> bool {
        if let Some(remaining) = self.bytes.checked_sub(bytes) {
            self.bytes = remaining;
            true
        } else {
            self.truncated = true;
            false
        }
    }
    fn text(&mut self, value: &str) -> String {
        let mut text = String::new();
        for (offset, ch) in value.char_indices() {
            let bytes = ch.len_utf8();
            if offset.saturating_add(bytes) > MAX_UI_VALUE_BYTES {
                self.truncated = true;
                break;
            }
            if ch.is_control() && ch != '\n' && ch != '\t' {
                self.truncated = true;
                continue;
            }
            let encoded = if matches!(ch, '"' | '\\' | '\n' | '\t') {
                2
            } else {
                bytes
            };
            if !self.reserve(encoded) {
                break;
            }
            text.push(ch);
        }
        text
    }
}
