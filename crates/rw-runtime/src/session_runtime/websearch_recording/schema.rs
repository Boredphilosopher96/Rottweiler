//! Stored occurrences use closed source DTOs; upstream API decoding is independent.
use rw_tools::{WebSearchResponse, WebSearchResult, WebSearchSource};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(remote = "WebSearchResult", deny_unknown_fields)]
struct ResultFields {
    title: String,
    url: String,
    snippet: String,
}

#[derive(Deserialize)]
struct RecordedResult(#[serde(with = "ResultFields")] WebSearchResult);

#[derive(Deserialize)]
#[serde(remote = "WebSearchResponse", deny_unknown_fields)]
struct ResponseFields {
    source: WebSearchSource,
    #[serde(deserialize_with = "results")]
    results: Vec<WebSearchResult>,
}

#[derive(Deserialize)]
struct RecordedResponse(#[serde(with = "ResponseFields")] WebSearchResponse);

fn results<'de, D: Deserializer<'de>>(decoder: D) -> Result<Vec<WebSearchResult>, D::Error> {
    Vec::<RecordedResult>::deserialize(decoder)
        .map(|results| results.into_iter().map(|result| result.0).collect())
}

pub(super) fn decode(
    bytes: &[u8],
) -> Result<BTreeMap<String, Vec<WebSearchResponse>>, serde_json::Error> {
    admit(bytes)?;
    let recorded: BTreeMap<String, Vec<RecordedResponse>> = serde_json::from_slice(bytes)?;
    Ok(recorded
        .into_iter()
        .map(|(key, responses)| {
            (
                key,
                responses.into_iter().map(|response| response.0).collect(),
            )
        })
        .collect())
}

pub(super) fn admit(bytes: &[u8]) -> Result<(), serde_json::Error> {
    let shape = rw_types::json_structure::preflight_json(
        bytes,
        rw_types::json_structure::JsonStructureLimits {
            max_encoded_bytes: rw_providers::MAX_RECORDING_FIXTURE_BYTES,
            max_nodes: rw_providers::MAX_RECORDING_FIXTURE_BYTES / 16,
            max_string_bytes: rw_providers::MAX_RECORDING_FIXTURE_BYTES,
            max_depth: 64,
        },
    )?;
    if shape
        .decode_bytes::<RecordedResponse>()
        .is_none_or(|bytes| bytes > 4 * rw_providers::MAX_RECORDING_FIXTURE_BYTES)
    {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "web-search fixture decoded admission exceeded",
        ));
    }
    Ok(())
}

// Closed strings, vectors and map entries; no tagged/untagged trial payloads.
// The common profile also covers conversion from the recording wrappers into
// source DTO containers while their original containers remain alive.
impl rw_types::allocation::DecodeAllocation for RecordedResponse {
    fn decode_node_bytes() -> Option<usize> {
        Some(128)
    }
}
