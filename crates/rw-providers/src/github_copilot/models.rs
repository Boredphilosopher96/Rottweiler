use std::collections::BTreeMap;

use rw_types::config::ThinkingLevel;
use serde::Deserialize;
use serde_json::Value;

use crate::{ModelPricing, ProviderError, ProviderErrorKind};

/// GitHub Copilot inference dialect selected from the live model catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHubCopilotEndpoint {
    /// Anthropic-compatible `/v1/messages`.
    Messages,
    /// OpenAI-compatible `/responses`.
    Responses,
    /// OpenAI-compatible `/chat/completions`.
    ChatCompletions,
}

/// Token-price metadata returned by GitHub Copilot `/models`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GitHubCopilotPricing {
    /// Number of tokens in one provider billing batch.
    pub batch_size: u64,
    /// AI Credits per input batch.
    pub input_ai_credits_per_batch: f64,
    /// AI Credits per cached-input batch.
    pub cache_read_ai_credits_per_batch: f64,
    /// AI Credits per output batch.
    pub output_ai_credits_per_batch: f64,
}

impl GitHubCopilotPricing {
    /// Converts catalog prices into Rottweiler's per-million micro-USD form.
    ///
    /// GitHub defines one AI Credit as one US cent. This is a usage estimate;
    /// subscription allowances and organization policy are applied by GitHub.
    ///
    /// # Errors
    ///
    /// Returns an error for zero batches, negative/non-finite values, or overflow.
    pub fn to_model_pricing(
        self,
        display_name: String,
        max_context_tokens: u64,
        max_output_tokens: u64,
        supports_tools: bool,
        reasoning_efforts: Vec<ThinkingLevel>,
    ) -> Result<ModelPricing, ProviderError> {
        Ok(ModelPricing {
            display_name,
            max_context_tokens: Some(max_context_tokens),
            max_output_tokens: Some(max_output_tokens),
            supports_tools,
            supports_thinking: !reasoning_efforts.is_empty(),
            supports_vision: false,
            reasoning_efforts,
            input_per_million_micros_usd: github_copilot_micros_usd_per_million(
                self.input_ai_credits_per_batch,
                self.batch_size,
            )?,
            output_per_million_micros_usd: github_copilot_micros_usd_per_million(
                self.output_ai_credits_per_batch,
                self.batch_size,
            )?,
            cache_read_per_million_micros_usd: Some(github_copilot_micros_usd_per_million(
                self.cache_read_ai_credits_per_batch,
                self.batch_size,
            )?),
            // `/models` exposes cached-input reads, not cache-write pricing.
            cache_write_per_million_micros_usd: None,
            reasoning_per_million_micros_usd: None,
        })
    }
}

/// One usable model from GitHub Copilot's authenticated catalog.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq)]
pub struct GitHubCopilotModel {
    /// Provider-local model id used in inference requests.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Provider family marker.
    pub family: String,
    /// Highest-priority supported raw dialect.
    pub endpoint: GitHubCopilotEndpoint,
    /// Whether the authenticated account exposes this model in its picker.
    pub picker_enabled: bool,
    /// Maximum context window.
    pub max_context_tokens: u64,
    /// Maximum prompt tokens.
    pub max_prompt_tokens: u64,
    /// Maximum output tokens.
    pub max_output_tokens: u64,
    /// Function-call support.
    pub supports_tools: bool,
    /// Image-input support.
    pub supports_vision: bool,
    /// Exact reasoning efforts understood by Rottweiler.
    pub reasoning_efforts: Vec<ThinkingLevel>,
    /// Optional explicit-thinking minimum.
    pub min_thinking_budget: Option<u32>,
    /// Optional explicit-thinking maximum.
    pub max_thinking_budget: Option<u32>,
    /// Whether adaptive thinking is supported.
    pub adaptive_thinking: bool,
    /// Optional authenticated catalog pricing.
    pub pricing: Option<GitHubCopilotPricing>,
}

impl GitHubCopilotModel {
    /// Converts optional Copilot prices into Rottweiler's pricing schema.
    ///
    /// # Errors
    ///
    /// Returns an error when upstream pricing is invalid or overflows.
    pub fn model_pricing(&self) -> Result<Option<ModelPricing>, ProviderError> {
        self.pricing
            .map(|pricing| {
                pricing.to_model_pricing(
                    self.name.clone(),
                    self.max_context_tokens,
                    self.max_output_tokens,
                    self.supports_tools,
                    self.reasoning_efforts.clone(),
                )
            })
            .transpose()
    }
}

/// Authenticated GitHub Copilot model snapshot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitHubCopilotCatalog {
    models: BTreeMap<String, GitHubCopilotModel>,
}

impl GitHubCopilotCatalog {
    /// Looks up a provider-local model id.
    #[must_use]
    pub fn get(&self, model_id: &str) -> Option<&GitHubCopilotModel> {
        self.models.get(model_id)
    }

    /// Iterates usable models in deterministic id order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &GitHubCopilotModel)> {
        self.models.iter().map(|(id, model)| (id.as_str(), model))
    }

    /// Number of usable (including non-picker utility) models.
    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// Whether no usable models were returned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// Parses a captured `/models` response without opening a socket.
///
/// Policy-disabled, malformed, endpoint-less, and capability-incomplete
/// records are skipped independently so one future record cannot hide valid
/// models. A valid response with no usable models fails closed.
///
/// # Errors
///
/// Returns a sanitized protocol error for an invalid envelope or empty result.
pub fn parse_github_copilot_models(bytes: &[u8]) -> Result<GitHubCopilotCatalog, ProviderError> {
    let response: ModelsResponse = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "GitHub Copilot model discovery returned an invalid response",
        )
    })?;
    let models = response
        .data
        .into_iter()
        .filter_map(|raw| serde_json::from_value::<RemoteModel>(raw).ok())
        .filter_map(build_model)
        .map(|model| (model.id.clone(), model))
        .collect::<BTreeMap<_, _>>();
    if models.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::Unsupported,
            "GitHub Copilot returned no policy-enabled models with complete capabilities",
        ));
    }
    Ok(GitHubCopilotCatalog { models })
}

/// Converts tokens at an authenticated catalog price into AI Credits.
///
/// # Errors
///
/// Returns an error for a zero batch or invalid price.
#[allow(clippy::cast_precision_loss)]
pub fn github_copilot_ai_credits(
    tokens: u64,
    ai_credits_per_batch: f64,
    batch_size: u64,
) -> Result<f64, ProviderError> {
    validate_price(ai_credits_per_batch, batch_size)?;
    let result = (tokens as f64) * ai_credits_per_batch / (batch_size as f64);
    if result.is_finite() {
        Ok(result)
    } else {
        Err(invalid_price())
    }
}

/// Converts an AI-Credit batch rate to micro-USD per million tokens.
///
/// One AI Credit is one US cent, or 10,000 micro-USD.
///
/// # Errors
///
/// Returns an error for a zero batch, invalid price, or integer overflow.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub fn github_copilot_micros_usd_per_million(
    ai_credits_per_batch: f64,
    batch_size: u64,
) -> Result<u64, ProviderError> {
    validate_price(ai_credits_per_batch, batch_size)?;
    let micros = ai_credits_per_batch * 10_000.0 * 1_000_000.0 / (batch_size as f64);
    if !micros.is_finite() || !(0.0..=(u64::MAX as f64)).contains(&micros) {
        return Err(invalid_price());
    }
    Ok(micros.round() as u64)
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<Value>,
}

#[derive(Deserialize)]
struct RemoteModel {
    model_picker_enabled: bool,
    id: String,
    name: String,
    #[serde(rename = "version")]
    _version: String,
    supported_endpoints: Option<Vec<String>>,
    policy: Option<RemotePolicy>,
    billing: Option<RemoteBilling>,
    capabilities: RemoteCapabilities,
}

#[derive(Deserialize)]
struct RemotePolicy {
    state: Option<String>,
}

#[derive(Deserialize)]
struct RemoteBilling {
    token_prices: Option<RemoteTokenPrices>,
}

#[derive(Deserialize)]
struct RemoteTokenPrices {
    batch_size: u64,
    default: RemotePrices,
}

#[derive(Deserialize)]
struct RemotePrices {
    #[serde(rename = "cache_price")]
    cache: f64,
    #[serde(rename = "input_price")]
    input: f64,
    #[serde(rename = "output_price")]
    output: f64,
}

#[derive(Deserialize)]
struct RemoteCapabilities {
    family: String,
    limits: Option<RemoteLimits>,
    supports: RemoteSupports,
}

#[derive(Deserialize)]
struct RemoteLimits {
    max_context_window_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
    max_prompt_tokens: Option<u64>,
    vision: Option<RemoteVision>,
}

#[derive(Deserialize)]
struct RemoteVision {
    #[serde(default)]
    supported_media_types: Vec<String>,
}

#[derive(Deserialize)]
struct RemoteSupports {
    adaptive_thinking: Option<bool>,
    max_thinking_budget: Option<u32>,
    min_thinking_budget: Option<u32>,
    reasoning_effort: Option<Vec<String>>,
    tool_calls: Option<bool>,
    vision: Option<bool>,
}

fn build_model(remote: RemoteModel) -> Option<GitHubCopilotModel> {
    if remote.id.trim().is_empty()
        || remote.name.trim().is_empty()
        || remote.capabilities.family.trim().is_empty()
        || remote
            .policy
            .as_ref()
            .and_then(|policy| policy.state.as_deref())
            == Some("disabled")
    {
        return None;
    }
    let limits = remote.capabilities.limits?;
    let max_prompt_tokens = limits.max_prompt_tokens?;
    let max_output_tokens = limits.max_output_tokens?;
    let supports_tools = remote.capabilities.supports.tool_calls?;
    let endpoints = remote.supported_endpoints.as_deref().unwrap_or_default();
    let endpoint = if endpoints.iter().any(|endpoint| endpoint == "/v1/messages") {
        GitHubCopilotEndpoint::Messages
    } else if endpoints.iter().any(|endpoint| endpoint == "/responses") {
        GitHubCopilotEndpoint::Responses
    } else if endpoints
        .iter()
        .any(|endpoint| endpoint == "/chat/completions")
    {
        GitHubCopilotEndpoint::ChatCompletions
    } else {
        return None;
    };
    let reasoning_efforts = parse_reasoning_efforts(
        remote.capabilities.supports.reasoning_effort.as_deref(),
        remote.capabilities.supports.adaptive_thinking == Some(true),
        remote.capabilities.supports.max_thinking_budget,
    );
    let supports_vision = remote.capabilities.supports.vision.unwrap_or(false)
        || limits.vision.as_ref().is_some_and(|vision| {
            vision
                .supported_media_types
                .iter()
                .any(|kind| kind.starts_with("image/"))
        });
    let pricing = remote
        .billing
        .and_then(|billing| billing.token_prices)
        .map(|prices| GitHubCopilotPricing {
            batch_size: prices.batch_size,
            input_ai_credits_per_batch: prices.default.input,
            cache_read_ai_credits_per_batch: prices.default.cache,
            output_ai_credits_per_batch: prices.default.output,
        });
    Some(GitHubCopilotModel {
        id: remote.id,
        name: remote.name,
        family: remote.capabilities.family,
        endpoint,
        picker_enabled: remote.model_picker_enabled,
        max_context_tokens: limits
            .max_context_window_tokens
            .unwrap_or(max_prompt_tokens),
        max_prompt_tokens,
        max_output_tokens,
        supports_tools,
        supports_vision,
        reasoning_efforts,
        min_thinking_budget: remote.capabilities.supports.min_thinking_budget,
        max_thinking_budget: remote.capabilities.supports.max_thinking_budget,
        adaptive_thinking: remote
            .capabilities
            .supports
            .adaptive_thinking
            .unwrap_or(false),
        pricing,
    })
}

fn parse_reasoning_efforts(
    efforts: Option<&[String]>,
    adaptive: bool,
    max_budget: Option<u32>,
) -> Vec<ThinkingLevel> {
    let mut parsed = efforts
        .unwrap_or_default()
        .iter()
        .filter_map(|effort| match effort.as_str() {
            "none" => Some(ThinkingLevel::Off),
            "low" | "minimal" => Some(ThinkingLevel::Low),
            "medium" => Some(ThinkingLevel::Medium),
            "high" | "xhigh" => Some(ThinkingLevel::High),
            _ => None,
        })
        .collect::<Vec<_>>();
    if parsed.is_empty() && (adaptive || max_budget.is_some()) {
        parsed.extend([
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ]);
    }
    parsed.sort_by_key(|effort| match effort {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Low => 1,
        ThinkingLevel::Medium => 2,
        ThinkingLevel::High => 3,
    });
    parsed.dedup();
    parsed
}

fn validate_price(ai_credits_per_batch: f64, batch_size: u64) -> Result<(), ProviderError> {
    if batch_size == 0 || !ai_credits_per_batch.is_finite() || ai_credits_per_batch < 0.0 {
        Err(invalid_price())
    } else {
        Ok(())
    }
}

fn invalid_price() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        "GitHub Copilot model catalog contained invalid token pricing",
    )
}

#[cfg(test)]
mod tests {
    use rw_types::config::ThinkingLevel;
    use serde_json::json;

    use crate::ProviderErrorKind;

    use super::{
        GitHubCopilotEndpoint, github_copilot_ai_credits, github_copilot_micros_usd_per_million,
        parse_github_copilot_models,
    };

    fn model(
        id: &str,
        picker: bool,
        policy: &str,
        endpoints: &[&str],
        complete: bool,
    ) -> serde_json::Value {
        json!({
            "model_picker_enabled": picker,
            "id": id,
            "name": format!("Name {id}"),
            "version": format!("{id}-2026-07-10"),
            "supported_endpoints": endpoints,
            "policy": { "state": policy },
            "billing": {
                "token_prices": {
                    "batch_size": 1_000_000,
                    "default": {
                        "cache_price": 25.0,
                        "input_price": 250.0,
                        "output_price": 1500.0
                    }
                }
            },
            "capabilities": {
                "family": "fixture-family",
                "limits": {
                    "max_context_window_tokens": 200_000,
                    "max_output_tokens": 32_000,
                    "max_prompt_tokens": if complete { json!(168_000) } else { serde_json::Value::Null },
                    "vision": { "supported_media_types": ["image/png"] }
                },
                "supports": {
                    "adaptive_thinking": true,
                    "reasoning_effort": ["low", "medium", "high", "future"],
                    "tool_calls": true,
                    "vision": false
                }
            }
        })
    }

    #[test]
    fn discovery_filters_policy_and_incomplete_models_and_prioritizes_messages() {
        let bytes = serde_json::to_vec(&json!({
            "data": [
                model("priority", true, "enabled", &["/chat/completions", "/responses", "/v1/messages"], true),
                model("disabled", true, "disabled", &["/responses"], true),
                model("incomplete", true, "enabled", &["/responses"], false),
                model("utility", false, "enabled", &["/chat/completions"], true),
                { "future": "shape" }
            ]
        }))
        .unwrap_or_else(|error| panic!("fixture must serialize: {error}"));
        let catalog = parse_github_copilot_models(&bytes)
            .unwrap_or_else(|error| panic!("catalog must parse: {error}"));
        assert_eq!(catalog.len(), 2);
        let priority = catalog
            .get("priority")
            .unwrap_or_else(|| panic!("priority model must exist"));
        assert_eq!(priority.endpoint, GitHubCopilotEndpoint::Messages);
        assert!(priority.supports_vision);
        assert_eq!(
            priority.reasoning_efforts,
            vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High
            ]
        );
        assert_eq!(
            catalog.get("utility").map(|model| model.picker_enabled),
            Some(false)
        );
        assert!(catalog.get("disabled").is_none());
        assert!(catalog.get("incomplete").is_none());
    }

    #[test]
    fn pricing_converts_ai_credits_without_floating_point_cost_drift() {
        assert_eq!(
            github_copilot_micros_usd_per_million(250.0, 1_000_000),
            Ok(2_500_000)
        );
        assert_eq!(github_copilot_ai_credits(20_000, 250.0, 1_000_000), Ok(5.0));
        let Err(error) = github_copilot_micros_usd_per_million(f64::NAN, 1) else {
            panic!("invalid price must fail");
        };
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
    }

    #[test]
    fn empty_usable_catalog_fails_closed_without_echoing_payload() {
        let secret_marker = "private-model-marker";
        let bytes = serde_json::to_vec(&json!({
            "data": [model(secret_marker, true, "disabled", &["/responses"], true)]
        }))
        .unwrap_or_else(|error| panic!("fixture must serialize: {error}"));
        let Err(error) = parse_github_copilot_models(&bytes) else {
            panic!("disabled-only catalog must fail");
        };
        assert_eq!(error.kind, ProviderErrorKind::Unsupported);
        assert!(!error.to_string().contains(secret_marker));
    }
}
