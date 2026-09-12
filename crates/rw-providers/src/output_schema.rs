//! Finite provider-neutral structured output schemas.
mod owner;
mod validate;
pub(crate) mod wire;
pub use owner::OutputValidation;
pub(crate) use owner::fingerprint;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_OUTPUT_SCHEMA_NODES: usize = 256;
pub const MAX_OUTPUT_SCHEMA_DEPTH: usize = 16;
pub const MAX_OUTPUT_SCHEMA_BYTES: usize = 64 * 1024;
pub const MAX_STRUCTURED_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_STRUCTURED_OUTPUT_NODES: usize = 4_096;

/// Required output semantics for one provider request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputContract {
    /// Ordinary streaming prose, reasoning and tool workflows.
    Text {},
    /// One complete JSON object satisfying the finite schema.
    JsonSchema {
        /// ASCII schema identifier, one to 64 letters, digits, underscores or hyphens.
        name: String,
        schema: OutputSchema,
    },
}

/// A closed schema: every object field is required and extra keys are forbidden.
/// Nullable values represent optional data without omitting its field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputSchema {
    String {},
    Number {},
    Integer {},
    Boolean {},
    Null {},
    Array { items: Box<OutputSchema> },
    Object { fields: Vec<OutputField> },
    Nullable { value: Box<OutputSchema> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct OutputField {
    pub name: String,
    pub schema: OutputSchema,
}

impl OutputContract {
    /// Validate bounded schema structure before discovery, authentication or inference.
    /// # Errors
    /// Rejects invalid names, duplicate fields, excessive depth/size, and non-object roots.
    pub fn validate(&self) -> Result<(), crate::ProviderError> {
        let Self::JsonSchema { name, schema } = self else {
            return Ok(());
        };
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || !matches!(schema, OutputSchema::Object { .. })
        {
            return Err(invalid());
        }
        let mut nodes = 0;
        let mut bytes = name.capacity();
        schema.check(1, &mut nodes, &mut bytes)
    }
}

impl OutputSchema {
    fn check(
        &self,
        depth: usize,
        nodes: &mut usize,
        bytes: &mut usize,
    ) -> Result<(), crate::ProviderError> {
        *nodes += 1;
        if depth > MAX_OUTPUT_SCHEMA_DEPTH || *nodes > MAX_OUTPUT_SCHEMA_NODES {
            return Err(invalid());
        }
        match self {
            Self::Array { items } | Self::Nullable { value: items } => {
                *bytes = bytes
                    .checked_add(std::mem::size_of::<Self>())
                    .ok_or_else(invalid)?;
                items.check(depth + 1, nodes, bytes)?;
            }
            Self::Object { fields } => {
                if fields.len() > 64 {
                    return Err(invalid());
                }
                *bytes = bytes
                    .checked_add(
                        fields
                            .capacity()
                            .checked_mul(std::mem::size_of::<OutputField>())
                            .ok_or_else(invalid)?,
                    )
                    .ok_or_else(invalid)?;
                for (index, field) in fields.iter().enumerate() {
                    if field.name.is_empty()
                        || field.name.len() > 128
                        || fields[..index]
                            .iter()
                            .any(|earlier| earlier.name == field.name)
                    {
                        return Err(invalid());
                    }
                    *bytes = bytes
                        .checked_add(field.name.capacity())
                        .ok_or_else(invalid)?;
                    field.schema.check(depth + 1, nodes, bytes)?;
                }
            }
            Self::String {}
            | Self::Number {}
            | Self::Integer {}
            | Self::Boolean {}
            | Self::Null {} => {}
        }
        if *bytes > MAX_OUTPUT_SCHEMA_BYTES {
            return Err(invalid());
        }
        Ok(())
    }
}

fn invalid() -> crate::ProviderError {
    crate::ProviderError::new(
        crate::ProviderErrorKind::InvalidRequest,
        "structured output schema exceeds its finite contract",
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod adapter_tests;
