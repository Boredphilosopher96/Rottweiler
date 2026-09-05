//! Required-nullable and nested collection shapes project exact display limits.
use schemars::{Schema, SchemaGenerator, json_schema};
use serde_json::{Value, json};
fn display_text(limit: usize) -> Value {
    json!({"type":"string","maxLength":limit,"x-rw-max-utf8-bytes":limit})
}
pub(super) fn nullable_display_value(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({"anyOf":[{"type":"null"},display_text(super::MAX_UI_VALUE_BYTES)]})
}
pub(super) fn bounded_display_list(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"array","maxItems":super::MAX_UI_LIST_ITEMS,"items":display_text(super::MAX_UI_VALUE_BYTES)})
}
pub(super) fn bounded_display_table(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"array","maxItems":super::MAX_UI_TABLE_ROWS,"items":{"type":"array","maxItems":super::MAX_UI_TABLE_COLUMNS,"items":display_text(super::MAX_UI_VALUE_BYTES)}})
}
pub(super) fn bounded_column_labels(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"array","minItems":1,"maxItems":super::MAX_UI_TABLE_COLUMNS,"items":display_text(super::MAX_UI_LABEL_BYTES)})
}
