//! The cache owns header annotations, never a second full tool schema.
use super::{MAX_BYTES, MAX_HEADERS, McpError, invalid};
use crate::payload_work::Allocation;
use rmcp::model::JsonObject;
use serde_json::Value;

pub(crate) struct ToolHeaderAnnotations {
    pub(super) pairs: Vec<(String, String)>,
    _retained: Allocation,
}
impl ToolHeaderAnnotations {
    /// None rejects only the malformed remote tool. Err is local admission failure.
    pub(crate) fn extract(schema: &JsonObject) -> Result<Option<Self>, McpError> {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return Ok(Some(Self {
                pairs: Vec::new(),
                _retained: Allocation::new(0)?,
            }));
        };
        let mut count = 0usize;
        let mut bytes = 0usize;
        let mut work = 0usize;
        for (property, schema) in properties {
            if !nested_valid(schema, 0, &mut work) {
                return Ok(None);
            }
            let Some(raw) = schema.get("x-mcp-header") else {
                continue;
            };
            let Some(header) = raw
                .as_str()
                .filter(|h| !h.is_empty() && h.bytes().all(tchar))
            else {
                return Ok(None);
            };
            if !matches!(
                schema.get("type").and_then(Value::as_str),
                Some("string" | "integer" | "boolean")
            ) {
                return Ok(None);
            }
            // No lowercase copy or temporary set is needed for <=32 annotations.
            if properties
                .iter()
                .take_while(|(key, _)| *key != property)
                .filter_map(|(_, earlier)| earlier.get("x-mcp-header").and_then(Value::as_str))
                .any(|earlier| earlier.eq_ignore_ascii_case(header))
            {
                return Ok(None);
            }
            count += 1;
            bytes = bytes
                .checked_add(property.len())
                .and_then(|n| n.checked_add(header.len() + "mcp-param-".len()))
                .ok_or_else(invalid)?;
            if count > MAX_HEADERS || bytes > MAX_BYTES {
                return Err(invalid());
            }
        }
        let retained =
            Allocation::new(bytes * 2 + count * std::mem::size_of::<(String, String)>() + 4096)?;
        let mut pairs = Vec::with_capacity(count);
        for (property, schema) in properties {
            if let Some(header) = schema.get("x-mcp-header").and_then(Value::as_str) {
                let mut name = String::with_capacity("mcp-param-".len() + header.len());
                name.push_str("mcp-param-");
                name.push_str(header);
                name.make_ascii_lowercase();
                pairs.push((property.clone(), name));
            }
        }
        Ok(Some(Self {
            pairs,
            _retained: retained,
        }))
    }
}
fn nested_valid(schema: &Value, depth: usize, work: &mut usize) -> bool {
    *work += 1;
    if depth > 64 || *work > 65_536 {
        return false;
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for schema in properties.values() {
            if schema.get("x-mcp-header").is_some() || !nested_valid(schema, depth + 1, work) {
                return false;
            }
        }
    }
    true
}
fn tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}
