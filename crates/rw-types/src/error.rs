use std::error::Error as StdError;

use thiserror::Error as ThisError;

use crate::EngineErrorCategory;

/// Source-preserving SDK error shared by headless engine consumers.
#[derive(Debug, ThisError)]
#[error("{message}")]
pub struct Error {
    category: EngineErrorCategory,
    message: String,
    #[source]
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    /// Creates a categorized error without an underlying source.
    #[must_use]
    pub fn new(category: EngineErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a categorized error while preserving its source chain.
    #[must_use]
    pub fn with_source<E>(
        category: EngineErrorCategory,
        message: impl Into<String>,
        source: E,
    ) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self {
            category,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Returns the stable category sent across protocol boundaries.
    #[must_use]
    pub const fn category(&self) -> &EngineErrorCategory {
        &self.category
    }

    /// Returns the client-safe summary without formatting the source chain.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}
