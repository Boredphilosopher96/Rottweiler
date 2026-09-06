//! JSON Schema shapes for required wire values that may explicitly be null.

use schemars::{JsonSchema, Schema, SchemaGenerator};

/// Used with `schema_with` on a field whose deserializer requires its key.
/// The generated field is required while its value accepts the same null as `Option<T>`.
#[must_use]
pub fn required_nullable<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    Option::<T>::json_schema(generator)
}
