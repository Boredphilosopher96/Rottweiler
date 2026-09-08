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
