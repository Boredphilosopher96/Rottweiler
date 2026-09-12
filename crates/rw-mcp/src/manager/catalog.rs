//! Catalog validation and identity retain the decoded source through actual CPU work.
use super::{MAX_CATALOG_ENTRIES, MAX_CATALOG_ENTRY_BYTES, sanitize_catalog};
use crate::{McpError, McpResponse};
use serde_json::Value;

pub(super) struct PreparedCatalog {
    pub(super) values: McpResponse<Vec<Value>>,
    pub(super) fingerprint: blake3::Hash,
}

pub(super) async fn prepare_catalog(
    values: McpResponse<Vec<Value>>,
) -> Result<PreparedCatalog, McpError> {
    rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || {
        let values = sanitize_catalog(values)?;
        let fingerprint = fingerprint(&values)?;
        Ok(PreparedCatalog {
            values,
            fingerprint,
        })
    })
    .await
    .map_err(|_| McpError::Protocol("MCP catalog preparation failed".into()))?
}

fn fingerprint(values: &[Value]) -> Result<blake3::Hash, McpError> {
    let mut hash = blake3::Hasher::new();
    rw_types::json_encoding::JsonWriter::stream(
        &mut hash,
        MAX_CATALOG_ENTRIES * (MAX_CATALOG_ENTRY_BYTES + 1) + 2,
    )
    .serialize(values)
    .map_err(|_| McpError::Protocol("MCP catalog identity exceeds its contract".into()))?;
    Ok(hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_identity_matches_exact_json_and_refuses_oversized_catalog()
    -> Result<(), Box<dyn std::error::Error>> {
        let values = vec![serde_json::json!({"name":"tool", "schema":{"enum":["α", "x\n"]}})];
        assert_eq!(
            fingerprint(&values)?,
            blake3::hash(&serde_json::to_vec(&values)?)
        );
        let oversized = vec![Value::String(
            "x".repeat(MAX_CATALOG_ENTRIES * (MAX_CATALOG_ENTRY_BYTES + 1)),
        )];
        assert!(fingerprint(&oversized).is_err());
        Ok(())
    }
}
