//! Refresh support for the public models.dev pricing catalog.

use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use reqwest::header::{DATE, ETAG};
use rw_types::config::ThinkingLevel;
use serde::Deserialize;
use serde_json::Number;
use url::{Host, Url};

use crate::{
    ModelPricing, PricingTable, ProviderError, ProviderErrorKind, ProxyAuthentication,
    ProxySettings,
    http::{build_client_with_proxy_auth, response_error, transport_error},
};

/// Default upstream catalog endpoint.
pub const DEFAULT_MODELS_DEV_URL: &str = "https://models.dev/api.json";

const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;

/// Summary returned after a catalog is validated and installed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelsRefreshReport {
    /// Number of priced canonical models installed.
    pub model_count: usize,
    /// Exact upstream URL recorded in the installed table.
    pub source_url: String,
    /// User-scoped destination path.
    pub path: PathBuf,
    /// Upstream `ETag` or deterministic body digest.
    pub revision: String,
}

/// Refreshes models.dev data through the global outbound proxy path.
///
/// The HTTP client has ambient proxy discovery disabled. `ProxySettings`
/// explicitly supplies the global/configured proxy before the environment
/// fallback (where `NO_PROXY` applies); provider overrides are ignored.
///
/// # Errors
///
/// Returns an error for unsafe source URLs, network failures, malformed data,
/// or an atomic installation failure. Validation completes before the existing
/// destination is touched.
pub async fn refresh_models_dev(
    source: &str,
    output: &Path,
    proxies: &ProxySettings,
) -> Result<ModelsRefreshReport, ProviderError> {
    refresh_models_dev_with_proxy_auth(source, output, proxies, None).await
}

/// Refreshes models.dev through an optionally authenticated global proxy.
///
/// # Errors
///
/// Returns the same validation, transport, and installation errors as
/// [`refresh_models_dev`], plus invalid proxy-authentication configuration.
pub async fn refresh_models_dev_with_proxy_auth(
    source: &str,
    output: &Path,
    proxies: &ProxySettings,
    proxy_authentication: Option<&ProxyAuthentication>,
) -> Result<ModelsRefreshReport, ProviderError> {
    crate::http::require_process_network()?;
    let source = parse_source_url(source)?;
    let proxy = proxies
        .resolve_global(&source)
        .map(|resolution| resolution.url);
    let client = build_client_with_proxy_auth(proxy.as_ref(), proxy_authentication)?;
    let response = client
        .get(source.clone())
        .send()
        .await
        .map_err(transport_error)?;
    if let Some(error) = response_error(&response) {
        return Err(error);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES)
    {
        return Err(catalog_error("catalog exceeds the 64 MiB size limit"));
    }

    let snapshot_date = response
        .headers()
        .get(DATE)
        .and_then(|value| value.to_str().ok())
        .and_then(http_date_to_utc_date)
        .unwrap_or_else(utc_date_today);
    let upstream_revision = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(transport_error)?;
        let next_length = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| catalog_error("catalog exceeds the 64 MiB size limit"))?;
        if u64::try_from(next_length).map_or(true, |length| length > MAX_CATALOG_BYTES) {
            return Err(catalog_error("catalog exceeds the 64 MiB size limit"));
        }
        body.extend_from_slice(&chunk);
    }
    let revision =
        upstream_revision.unwrap_or_else(|| format!("blake3:{}", blake3::hash(&body).to_hex()));
    let table = convert_models_dev(
        &body,
        source.as_str(),
        snapshot_date.as_str(),
        revision.as_str(),
    )?;
    let contents = table.to_toml()?;
    let installed = PricingTable::install_atomically(output, &contents).await?;

    Ok(ModelsRefreshReport {
        model_count: installed.models.len(),
        source_url: installed.source_url,
        path: output.to_owned(),
        revision: installed.revision,
    })
}

/// Returns the user-scoped pricing-table destination.
///
/// `ROTTWEILER_HOME` wins, then `XDG_CONFIG_HOME/rottweiler`, then
/// `HOME/.rottweiler`. Relative roots fail closed so an untrusted project can
/// never become the implicit destination.
///
/// # Errors
///
/// Returns an error when no absolute user configuration root is available.
pub fn default_models_path() -> Result<PathBuf, ProviderError> {
    let (name, root) = if let Some(root) = nonempty_env("ROTTWEILER_HOME") {
        ("ROTTWEILER_HOME", PathBuf::from(root))
    } else if let Some(root) = nonempty_env("XDG_CONFIG_HOME") {
        ("XDG_CONFIG_HOME", PathBuf::from(root).join("rottweiler"))
    } else if let Some(root) = nonempty_env("HOME") {
        ("HOME", PathBuf::from(root).join(".rottweiler"))
    } else {
        return Err(catalog_error(
            "could not determine user models directory; set ROTTWEILER_HOME, XDG_CONFIG_HOME, or HOME",
        ));
    };
    if !root.is_absolute() {
        return Err(catalog_error(format!(
            "{name} must be an absolute path, got {:?}",
            root.display().to_string()
        )));
    }
    Ok(root.join("models.toml"))
}

fn nonempty_env(name: &str) -> Option<std::ffi::OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

fn parse_source_url(value: &str) -> Result<Url, ProviderError> {
    let url = Url::parse(value).map_err(|_| catalog_error("source must be an absolute URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(catalog_error("source URL must not contain credentials"));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback(&url) => Ok(url),
        "http" => Err(catalog_error(
            "plaintext HTTP model sources are allowed only on loopback",
        )),
        _ => Err(catalog_error("source URL must use HTTPS")),
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamProvider {
    #[serde(default)]
    models: BTreeMap<String, UpstreamModel>,
}

#[derive(Debug, Deserialize)]
struct UpstreamModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    reasoning_options: Vec<UpstreamReasoningOption>,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    modalities: UpstreamModalities,
    #[serde(default)]
    limit: UpstreamLimit,
    cost: Option<UpstreamCost>,
}

#[derive(Debug, Default, Deserialize)]
struct UpstreamModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct UpstreamReasoningOption {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    values: Vec<Option<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct UpstreamLimit {
    context: Option<u64>,
    output: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct UpstreamCost {
    input: Option<Number>,
    output: Option<Number>,
    cache_read: Option<Number>,
    cache_write: Option<Number>,
    reasoning: Option<Number>,
}

fn convert_models_dev(
    json: &[u8],
    source_url: &str,
    snapshot_date: &str,
    revision: &str,
) -> Result<PricingTable, ProviderError> {
    let providers: BTreeMap<String, UpstreamProvider> = serde_json::from_slice(json)
        .map_err(|error| catalog_error(format!("invalid models.dev JSON: {error}")))?;
    let mut models = BTreeMap::new();

    for (provider_id, provider) in providers {
        for (model_id, model) in provider.models {
            let Some(cost) = model.cost else {
                continue;
            };
            if provider_id.trim().is_empty() || model_id.trim().is_empty() {
                return Err(catalog_error(
                    "priced provider and model ids must be non-empty",
                ));
            }
            let input = required_rate(cost.input, &provider_id, &model_id, "cost.input")?;
            let output = required_rate(cost.output, &provider_id, &model_id, "cost.output")?;
            let canonical = format!("{provider_id}/{model_id}");
            models.insert(
                canonical,
                ModelPricing {
                    display_name: if model.name.is_empty() {
                        model_id.clone()
                    } else {
                        model.name
                    },
                    max_context_tokens: model.limit.context,
                    max_output_tokens: model.limit.output,
                    supports_tools: model.tool_call,
                    supports_thinking: model.reasoning,
                    supports_vision: model
                        .modalities
                        .input
                        .iter()
                        .any(|modality| modality.eq_ignore_ascii_case("image")),
                    reasoning_efforts: reasoning_efforts(&model.reasoning_options),
                    input_per_million_micros_usd: input,
                    output_per_million_micros_usd: output,
                    cache_read_per_million_micros_usd: optional_rate(
                        cost.cache_read,
                        &provider_id,
                        &model_id,
                        "cost.cache_read",
                    )?,
                    cache_write_per_million_micros_usd: optional_rate(
                        cost.cache_write,
                        &provider_id,
                        &model_id,
                        "cost.cache_write",
                    )?,
                    reasoning_per_million_micros_usd: optional_rate(
                        cost.reasoning,
                        &provider_id,
                        &model_id,
                        "cost.reasoning",
                    )?,
                },
            );
        }
    }

    let table = PricingTable {
        source_url: source_url.to_owned(),
        snapshot_date: snapshot_date.to_owned(),
        revision: revision.to_owned(),
        models,
    };
    table.validate()?;
    Ok(table)
}

fn reasoning_efforts(options: &[UpstreamReasoningOption]) -> Vec<ThinkingLevel> {
    let mut efforts = Vec::new();
    for value in options
        .iter()
        .filter(|option| option.kind == "effort")
        .flat_map(|option| option.values.iter().flatten())
    {
        let effort = match value.as_str() {
            "none" => Some(ThinkingLevel::Off),
            "low" => Some(ThinkingLevel::Low),
            "medium" => Some(ThinkingLevel::Medium),
            "high" => Some(ThinkingLevel::High),
            _ => None,
        };
        if let Some(effort) = effort
            && !efforts.contains(&effort)
        {
            efforts.push(effort);
        }
    }
    efforts
}

fn required_rate(
    value: Option<Number>,
    provider: &str,
    model: &str,
    field: &str,
) -> Result<u64, ProviderError> {
    let value = value
        .ok_or_else(|| catalog_error(format!("{provider}/{model} is missing required {field}")))?;
    decimal_rate_to_micros(&value).map_err(|reason| {
        catalog_error(format!("invalid {field} for {provider}/{model}: {reason}"))
    })
}

fn optional_rate(
    value: Option<Number>,
    provider: &str,
    model: &str,
    field: &str,
) -> Result<Option<u64>, ProviderError> {
    value
        .map(|value| {
            decimal_rate_to_micros(&value).map_err(|reason| {
                catalog_error(format!("invalid {field} for {provider}/{model}: {reason}"))
            })
        })
        .transpose()
}

fn decimal_rate_to_micros(number: &Number) -> Result<u64, &'static str> {
    let rendered = number.to_string();
    if rendered.starts_with('-') {
        return Err("price cannot be negative");
    }
    let (mantissa, exponent) = rendered.split_once(['e', 'E']).map_or(
        (rendered.as_str(), 0_i32),
        |(mantissa, exponent)| {
            exponent
                .parse::<i32>()
                .map_or((mantissa, i32::MAX), |exponent| (mantissa, exponent))
        },
    );
    if exponent == i32::MAX {
        return Err("price exponent is out of range");
    }
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("price is not a decimal number");
    }
    let digits = format!("{whole}{fraction}");
    let significand = digits
        .parse::<u128>()
        .map_err(|_| "price is out of range")?;
    if significand == 0 {
        return Ok(0);
    }
    let fraction_digits = i32::try_from(fraction.len()).map_err(|_| "price is out of range")?;
    let scale = 6_i32
        .checked_add(exponent)
        .and_then(|scale| scale.checked_sub(fraction_digits))
        .ok_or("price is out of range")?;
    let micros = if scale >= 0 {
        significand
            .checked_mul(power_of_ten(
                u32::try_from(scale).map_err(|_| "price is out of range")?,
            )?)
            .ok_or("price is out of range")?
    } else {
        let divisor = power_of_ten(scale.unsigned_abs())?;
        let quotient = significand / divisor;
        let remainder = significand % divisor;
        // Upstream JSON occasionally exposes binary-float artifacts such as
        // 0.024999999999999998. Round non-negative rates to the nearest
        // micro-dollar, with exact halves rounded upward.
        if remainder.checked_mul(2).ok_or("price is out of range")? >= divisor {
            quotient.checked_add(1).ok_or("price is out of range")?
        } else {
            quotient
        }
    };
    u64::try_from(micros).map_err(|_| "price is out of range")
}

fn power_of_ten(exponent: u32) -> Result<u128, &'static str> {
    10_u128.checked_pow(exponent).ok_or("price is out of range")
}

fn http_date_to_utc_date(value: &str) -> Option<String> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 4 {
        return None;
    }
    let day = parts[1].parse::<u8>().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts[3].parse::<u16>().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn utc_date_today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() / 86_400);
    let days = i64::try_from(days).unwrap_or(i64::MAX);
    let (year, month, day) = civil_date_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days.saturating_add(719_468);
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn catalog_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

#[cfg(test)]
mod tests {
    use rw_types::config::ThinkingLevel;
    use serde_json::Number;

    use super::{convert_models_dev, decimal_rate_to_micros, parse_source_url};

    fn number(value: &str) -> Number {
        value
            .parse()
            .unwrap_or_else(|error| panic!("fixture number must parse: {error}"))
    }

    #[test]
    fn decimal_conversion_is_checked_and_exact() {
        assert_eq!(decimal_rate_to_micros(&number("3")), Ok(3_000_000));
        assert_eq!(decimal_rate_to_micros(&number("0.025")), Ok(25_000));
        assert_eq!(decimal_rate_to_micros(&number("2.5e-1")), Ok(250_000));
        assert_eq!(
            decimal_rate_to_micros(&number("0.024999999999999998")),
            Ok(25_000)
        );
        assert_eq!(decimal_rate_to_micros(&number("0.0000004")), Ok(0));
        assert_eq!(decimal_rate_to_micros(&number("0.0000005")), Ok(1));
        assert_eq!(
            decimal_rate_to_micros(&number("-1")),
            Err("price cannot be negative")
        );
    }

    #[test]
    fn current_flat_provider_shape_converts_capabilities_limits_and_costs() {
        let table = convert_models_dev(
            br#"{
              "fixture": {
                "id": "fixture",
                "models": {
                  "fast": {
                    "id": "fast",
                    "name": "Fixture Fast",
                    "reasoning": true,
                    "reasoning_options": [
                      {"type":"effort","values":["none","low","high","max",null]},
                      {"type":"budget_tokens"}
                    ],
                    "tool_call": true,
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "limit": {"context": 12345, "output": 678},
                    "cost": {"input": 0.25, "output": 2, "cache_read": 0.025, "reasoning": 0}
                  },
                  "unpriced": {"name": "Unavailable"}
                }
              }
            }"#,
            "https://models.dev/api.json",
            "2026-07-10",
            "fixture-revision",
        )
        .unwrap_or_else(|error| panic!("catalog must convert: {error}"));
        assert_eq!(table.models.len(), 1);
        let model = table
            .models
            .get("fixture/fast")
            .unwrap_or_else(|| panic!("canonical model expected"));
        assert_eq!(model.display_name, "Fixture Fast");
        assert_eq!(model.max_context_tokens, Some(12_345));
        assert_eq!(model.max_output_tokens, Some(678));
        assert!(model.supports_tools);
        assert!(model.supports_thinking);
        assert!(model.supports_vision);
        assert_eq!(
            model.reasoning_efforts,
            vec![ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High,]
        );
        assert_eq!(model.input_per_million_micros_usd, 250_000);
        assert_eq!(model.output_per_million_micros_usd, 2_000_000);
        assert_eq!(model.cache_read_per_million_micros_usd, Some(25_000));
        assert_eq!(model.cache_write_per_million_micros_usd, None);
        assert_eq!(model.reasoning_per_million_micros_usd, Some(0));
    }

    #[test]
    fn malformed_cost_is_rejected_and_remote_plaintext_is_unsafe() {
        let Err(error) = convert_models_dev(
            br#"{"fixture":{"models":{"bad":{"cost":{"input":1}}}}}"#,
            "https://models.dev/api.json",
            "2026-07-10",
            "bad",
        ) else {
            panic!("partial cost must fail");
        };
        assert!(error.message.contains("missing required cost.output"));

        let Err(error) = parse_source_url("http://models.example/api.json") else {
            panic!("remote plaintext source must fail");
        };
        assert!(error.message.contains("loopback"));
        assert!(parse_source_url("http://127.0.0.1:8000/api.json").is_ok());
    }
}
