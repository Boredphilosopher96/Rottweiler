use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Maximum encoded length of an MCP server identifier.
pub const MAX_MCP_SERVER_ID_BYTES: usize = 96;

/// JavaScript-compatible pattern for the MCP server identifier grammar.
pub const MCP_SERVER_ID_PATTERN: &str = r"^[A-Za-z0-9._-]{1,96}$";

/// Stable identifier for one configured MCP server.
///
/// Construction and deserialization both enforce the namespace grammar so an
/// accepted identifier cannot bypass the one canonical validator.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct McpServerId(String);

impl McpServerId {
    /// Parses a server identifier before it is used as a namespace.
    ///
    /// # Errors
    ///
    /// Returns [`McpServerIdError`] when the value is empty, oversized, or
    /// contains a byte outside the identifier grammar.
    pub fn new(value: impl Into<String>) -> Result<Self, McpServerIdError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    /// Constructs an identifier from a source-controlled static value.
    ///
    /// # Panics
    ///
    /// Panics when the static value violates the canonical identifier grammar.
    #[must_use]
    pub fn from_static(value: &'static str) -> Self {
        if let Err(error) = Self::validate(value) {
            panic!("invalid static MCP server id: {error}");
        }
        Self(value.to_owned())
    }

    /// Validates the canonical server identifier grammar.
    ///
    /// # Errors
    ///
    /// Returns [`McpServerIdError`] when the value is empty, oversized, or
    /// contains a byte outside the identifier grammar.
    pub fn validate(value: &str) -> Result<(), McpServerIdError> {
        if value.is_empty()
            || value.len() > MAX_MCP_SERVER_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(McpServerIdError(value.to_owned()));
        }
        Ok(())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for McpServerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for McpServerId {
    type Err = McpServerIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for McpServerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid MCP server id: {0}")]
pub struct McpServerIdError(String);

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn server_id_grammar_has_one_validating_constructor() {
        for valid in ["docs", "private.docs", "server_1", "server-1"] {
            let parsed: McpServerId = valid.parse().expect("valid server id");
            assert_eq!(parsed.as_str(), valid);
            assert_eq!(
                serde_json::from_str::<McpServerId>(&format!("\"{valid}\""))
                    .expect("valid serialized server id"),
                parsed
            );
        }

        for invalid in ["", "has space", "slash/name", &"x".repeat(97)] {
            assert!(McpServerId::new(invalid).is_err(), "accepted {invalid:?}");
            assert!(serde_json::from_str::<McpServerId>(&format!("\"{invalid}\"")).is_err());
        }
    }
}
