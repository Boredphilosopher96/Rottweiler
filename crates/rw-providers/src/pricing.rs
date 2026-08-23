use std::{collections::BTreeMap, path::Path};

use rw_types::config::ThinkingLevel;
use serde::{Deserialize, Serialize};

use crate::{ProviderError, ProviderErrorKind, TokenUsage};

/// Per-million-token prices in micro-US-dollars. Keeping rates integral avoids
/// floating-point drift in session budgets and replay fixtures.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    /// Human-readable model name from the data source.
    #[serde(default)]
    pub display_name: String,
    /// Advertised context window.
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    /// Advertised output limit.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// Tool-call capability marker.
    #[serde(default)]
    pub supports_tools: bool,
    /// Reasoning-control capability marker.
    #[serde(default)]
    pub supports_thinking: bool,
    /// Exact provider-neutral reasoning efforts advertised by the catalog.
    /// An empty list means the source did not provide a usable effort mapping.
    #[serde(default)]
    pub reasoning_efforts: Vec<ThinkingLevel>,
    /// Input price per million tokens.
    pub input_per_million_micros_usd: u64,
    /// Output price per million tokens.
    pub output_per_million_micros_usd: u64,
    /// Prompt-cache read price per million tokens. Missing inherits input;
    /// explicit zero means cache reads are free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_million_micros_usd: Option<u64>,
    /// Prompt-cache write price per million tokens. Missing inherits input;
    /// explicit zero means cache writes are free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million_micros_usd: Option<u64>,
    /// Separately billed reasoning price per million tokens. Missing inherits
    /// output; explicit zero means reasoning tokens are free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_per_million_micros_usd: Option<u64>,
}

/// Exact cost components and rounded total.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CostBreakdown {
    /// Input token cost.
    pub input_micros_usd: u64,
    /// Output token cost.
    pub output_micros_usd: u64,
    /// Cache-read token cost.
    pub cache_read_micros_usd: u64,
    /// Cache-write token cost.
    pub cache_write_micros_usd: u64,
    /// Separately reported reasoning token cost.
    pub reasoning_micros_usd: u64,
    /// Sum of all components.
    pub total_micros_usd: u64,
}

/// Refreshable pricing data keyed by canonical `provider/model` id.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingTable {
    /// Upstream refresh endpoint.
    pub source_url: String,
    /// UTC snapshot date for auditability.
    pub snapshot_date: String,
    /// Data revision/source marker.
    pub revision: String,
    /// Canonical model prices.
    pub models: BTreeMap<String, ModelPricing>,
}

impl PricingTable {
    /// Parses downloaded or cached TOML pricing data.
    ///
    /// # Errors
    ///
    /// Returns an error when the table does not match the schema.
    pub fn from_toml(contents: &str) -> Result<Self, ProviderError> {
        let table: Self = toml::from_str(contents).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("invalid pricing table: {error}"),
            )
        })?;
        table.validate()?;
        Ok(table)
    }

    /// Validates table metadata and canonical model entries.
    ///
    /// # Errors
    ///
    /// Returns an error when required metadata is missing or a model key is not
    /// a canonical `provider/model` identifier.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.source_url.trim().is_empty()
            || self.snapshot_date.trim().is_empty()
            || self.revision.trim().is_empty()
        {
            return Err(invalid_pricing(
                "source_url, snapshot_date, and revision must be non-empty",
            ));
        }
        if self.models.is_empty() {
            return Err(invalid_pricing("pricing table contains no priced models"));
        }
        if let Some(model) = self.models.keys().find(|model| {
            let Some((provider, model_id)) = model.split_once('/') else {
                return true;
            };
            provider.is_empty() || model_id.is_empty()
        }) {
            return Err(invalid_pricing(format!(
                "model key {model:?} is not a canonical provider/model identifier"
            )));
        }
        Ok(())
    }

    /// Serializes a validated table into deterministic TOML.
    ///
    /// # Errors
    ///
    /// Returns an error when validation or serialization fails.
    pub fn to_toml(&self) -> Result<String, ProviderError> {
        self.validate()?;
        toml::to_string_pretty(self)
            .map_err(|error| invalid_pricing(format!("could not serialize pricing table: {error}")))
    }

    /// Loads a refreshed pricing table from disk.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or parsed.
    pub async fn load(path: &Path) -> Result<Self, ProviderError> {
        let contents = tokio::fs::read_to_string(path).await.map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("could not read pricing table {}: {error}", path.display()),
            )
        })?;
        Self::from_toml(&contents)
    }

    /// Validates then atomically installs refreshed TOML data.
    ///
    /// # Errors
    ///
    /// Returns an error when validation or atomic replacement fails.
    pub async fn install_atomically(path: &Path, contents: &str) -> Result<Self, ProviderError> {
        let parsed = Self::from_toml(contents)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(pricing_io_error)?;
        }
        let temporary = path.with_extension("toml.tmp");
        tokio::fs::write(&temporary, contents)
            .await
            .map_err(pricing_io_error)?;
        tokio::fs::rename(&temporary, path)
            .await
            .map_err(pricing_io_error)?;
        Ok(parsed)
    }

    /// Computes exact integer cost, rounding each component to nearest micro-dollar.
    ///
    /// # Errors
    ///
    /// Returns an error if the cost exceeds the supported integer range.
    pub fn cost(
        &self,
        canonical_model: &str,
        usage: TokenUsage,
    ) -> Result<Option<CostBreakdown>, ProviderError> {
        let Some(rates) = self.models.get(canonical_model) else {
            return Ok(None);
        };
        let input = price(usage.input_tokens, rates.input_per_million_micros_usd)?;
        let output = price(usage.output_tokens, rates.output_per_million_micros_usd)?;
        let cache_read = price(
            usage.cache_read_tokens,
            rates
                .cache_read_per_million_micros_usd
                .unwrap_or(rates.input_per_million_micros_usd),
        )?;
        let cache_write = price(
            usage.cache_write_tokens,
            rates
                .cache_write_per_million_micros_usd
                .unwrap_or(rates.input_per_million_micros_usd),
        )?;
        let reasoning = price(
            usage.reasoning_tokens,
            rates
                .reasoning_per_million_micros_usd
                .unwrap_or(rates.output_per_million_micros_usd),
        )?;
        let total = input
            .checked_add(output)
            .and_then(|value| value.checked_add(cache_read))
            .and_then(|value| value.checked_add(cache_write))
            .and_then(|value| value.checked_add(reasoning))
            .ok_or_else(cost_overflow)?;
        Ok(Some(CostBreakdown {
            input_micros_usd: input,
            output_micros_usd: output,
            cache_read_micros_usd: cache_read,
            cache_write_micros_usd: cache_write,
            reasoning_micros_usd: reasoning,
            total_micros_usd: total,
        }))
    }
}

fn price(tokens: u64, rate: u64) -> Result<u64, ProviderError> {
    let rounded = u128::from(tokens)
        .checked_mul(u128::from(rate))
        .and_then(|value| value.checked_add(500_000))
        .ok_or_else(cost_overflow)?
        / 1_000_000;
    u64::try_from(rounded).map_err(|_| cost_overflow())
}

fn cost_overflow() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        "token cost exceeds the supported micro-dollar range",
    )
}

fn invalid_pricing(message: impl Into<String>) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        format!("invalid pricing table: {}", message.into()),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn pricing_io_error(error: std::io::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        format!("could not install pricing table: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use crate::TokenUsage;

    use super::PricingTable;

    #[test]
    fn recorded_session_cost_matches_hand_calculation() {
        let table = PricingTable::from_toml(
            r#"
revision = "fixture-1"
source_url = "https://models.dev/api.json"
snapshot_date = "2026-07-10"
[models."fixture/model"]
input_per_million_micros_usd = 3_000_000
output_per_million_micros_usd = 15_000_000
"#,
        )
        .unwrap_or_else(|error| panic!("pricing fixture must parse: {error}"));
        let cost = table
            .cost(
                "fixture/model",
                TokenUsage {
                    input_tokens: 1_000,
                    output_tokens: 200,
                    cache_read_tokens: 5_000,
                    cache_write_tokens: 400,
                    reasoning_tokens: 100,
                },
            )
            .unwrap_or_else(|error| panic!("cost must fit: {error}"))
            .unwrap_or_else(|| panic!("fixture price must exist"));
        // input 3000 + output 3000 + cache reads 15000 (input fallback)
        // + cache writes 1200 (input fallback) + reasoning 1500 (output fallback).
        assert_eq!(cost.input_micros_usd, 3_000);
        assert_eq!(cost.output_micros_usd, 3_000);
        assert_eq!(cost.cache_read_micros_usd, 15_000);
        assert_eq!(cost.cache_write_micros_usd, 1_200);
        assert_eq!(cost.reasoning_micros_usd, 1_500);
        assert_eq!(cost.total_micros_usd, 23_700);
    }

    #[test]
    fn explicit_zero_optional_rates_remain_free() {
        let table = PricingTable::from_toml(
            r#"
revision = "fixture-free"
source_url = "https://models.dev/api.json"
snapshot_date = "2026-07-10"
[models."fixture/free"]
input_per_million_micros_usd = 3_000_000
output_per_million_micros_usd = 15_000_000
cache_read_per_million_micros_usd = 0
cache_write_per_million_micros_usd = 0
reasoning_per_million_micros_usd = 0
"#,
        )
        .unwrap_or_else(|error| panic!("free-rate fixture must parse: {error}"));
        let cost = table
            .cost(
                "fixture/free",
                TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 10_000,
                    cache_write_tokens: 10_000,
                    reasoning_tokens: 10_000,
                },
            )
            .unwrap_or_else(|error| panic!("free rates must fit: {error}"))
            .unwrap_or_else(|| panic!("free-rate model must exist"));
        assert_eq!(cost.total_micros_usd, 0);
        let rates = &table.models["fixture/free"];
        assert_eq!(rates.cache_read_per_million_micros_usd, Some(0));
        assert_eq!(rates.cache_write_per_million_micros_usd, Some(0));
        assert_eq!(rates.reasoning_per_million_micros_usd, Some(0));
        let rendered = table
            .to_toml()
            .unwrap_or_else(|error| panic!("free rates must serialize: {error}"));
        let reparsed = PricingTable::from_toml(&rendered)
            .unwrap_or_else(|error| panic!("serialized free rates must parse: {error}"));
        assert_eq!(
            reparsed.models["fixture/free"].reasoning_per_million_micros_usd,
            Some(0)
        );
    }

    #[test]
    fn empty_cache_has_no_model_data() {
        let table = PricingTable::default();
        assert!(table.models.is_empty());
        let unknown = table
            .cost("missing/model", TokenUsage::default())
            .unwrap_or_else(|error| panic!("unknown lookup cannot overflow: {error}"));
        assert_eq!(unknown, None);
    }
}
