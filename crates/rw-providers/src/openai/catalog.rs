use super::{
    BTreeMap, CacheBreakpointSupport, Capabilities, DiscoveredModel,
    MAX_PROVIDER_MODEL_CATALOG_BYTES, ProviderError, ProviderErrorKind, StreamExt, Url, Value,
    WireMode, transport_error,
};

pub(super) fn discovery_endpoint(endpoint: &Url, subscription: bool) -> Result<Url, ProviderError> {
    if subscription && !is_loopback(endpoint) {
        let mut discovered =
            Url::parse(crate::OPENAI_SUBSCRIPTION_MODELS_ENDPOINT).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "built-in ChatGPT model catalog endpoint is invalid",
                )
            })?;
        append_subscription_catalog_version(&mut discovered);
        return Ok(discovered);
    }
    let path = endpoint.path();
    let base = path
        .strip_suffix("/chat/completions")
        .or_else(|| path.strip_suffix("/responses"))
        .unwrap_or_else(|| path.trim_end_matches('/'));
    let model_path = if subscription {
        "/backend-api/codex/models".to_owned()
    } else {
        format!("{base}/models")
    };
    let mut discovered = endpoint.clone();
    discovered.set_path(&model_path);
    discovered.set_query(None);
    discovered.set_fragment(None);
    if subscription {
        append_subscription_catalog_version(&mut discovered);
    }
    Ok(discovered)
}

pub(super) fn append_subscription_catalog_version(endpoint: &mut Url) {
    endpoint.query_pairs_mut().append_pair(
        "client_version",
        crate::OPENAI_SUBSCRIPTION_MODELS_COMPATIBILITY_VERSION,
    );
}

pub(super) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

pub(super) async fn bounded_catalog_bytes(
    response: reqwest::Response,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_MODEL_CATALOG_BYTES as u64)
    {
        return Err(model_catalog_too_large());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_PROVIDER_MODEL_CATALOG_BYTES {
            return Err(model_catalog_too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) fn model_catalog_too_large() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        "OpenAI-compatible model discovery response exceeded the size limit",
    )
}

pub(super) fn parse_openai_models(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "OpenAI-compatible model discovery returned invalid JSON",
        )
    })?;
    let data = envelope
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "OpenAI-compatible model discovery returned an invalid envelope",
            )
        })?;
    let models = data
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .filter_map(nonempty)
        .map(|id| DiscoveredModel {
            id: id.to_owned(),
            display_name: None,
            description: None,
            capabilities: None,
            pricing: None,
        })
        .map(|model| (model.id.clone(), model))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    Ok(models)
}

pub(super) fn parse_subscription_models(
    bytes: &[u8],
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "ChatGPT model discovery returned invalid JSON",
        )
    })?;
    let data = envelope
        .as_array()
        .or_else(|| envelope.get("models").and_then(Value::as_array))
        .or_else(|| envelope.get("data").and_then(Value::as_array))
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "ChatGPT model discovery returned an invalid envelope",
            )
        })?;
    Ok(data
        .iter()
        .filter(|model| {
            model
                .get("visibility")
                .and_then(Value::as_str)
                .is_none_or(|visibility| visibility == "list")
        })
        .filter_map(subscription_model)
        .map(|model| (model.id.clone(), model))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect())
}

pub(super) fn subscription_model(model: &Value) -> Option<DiscoveredModel> {
    let id = model
        .get("slug")
        .or_else(|| model.get("id"))
        .and_then(Value::as_str)
        .and_then(nonempty)?;
    let context = model
        .get("context_window")
        .or_else(|| model.get("max_context_tokens"))
        .and_then(Value::as_u64);
    let output = model.get("max_output_tokens").and_then(Value::as_u64);
    let vision = model
        .get("input_modalities")
        .and_then(Value::as_array)
        .is_some_and(|modalities| {
            modalities
                .iter()
                .any(|value| value.as_str() == Some("image"))
        });
    let thinking = model
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .is_some_and(|levels| !levels.is_empty());
    Some(DiscoveredModel {
        id: id.to_owned(),
        display_name: string_field(model, "display_name"),
        description: string_field(model, "description"),
        capabilities: Some(Capabilities {
            tool_calling: model
                .get("supports_tool_calls")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            vision,
            thinking,
            cache_breakpoints: CacheBreakpointSupport::Automatic,
            max_context_tokens: context,
            max_output_tokens: output,
            wire_mode: WireMode::OpenAiResponses,
        }),
        pricing: None,
    })
}

pub(super) fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(nonempty)
        .map(str::to_owned)
}

pub(super) fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}
