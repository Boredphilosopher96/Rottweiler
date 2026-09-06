//! Recording storage requires complete nested objects, independently of upstream DTO defaults.
use crate::{Capabilities, ModelPricing, ProviderError, ProviderModelMetadata, UsageAccounting};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(remote = "Capabilities", deny_unknown_fields)]
struct CapabilityFields {
    tool_calling: bool,
    vision: bool,
    thinking: bool,
    cache_breakpoints: crate::CacheBreakpointSupport,
    #[serde(deserialize_with = "Option::deserialize")]
    max_context_tokens: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    max_output_tokens: Option<u64>,
    wire_mode: crate::WireMode,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ModelPricing", deny_unknown_fields)]
struct PricingFields {
    display_name: String,
    #[serde(deserialize_with = "Option::deserialize")]
    max_context_tokens: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    max_output_tokens: Option<u64>,
    supports_tools: bool,
    supports_thinking: bool,
    supports_vision: bool,
    reasoning_efforts: Vec<rw_types::config::ThinkingLevel>,
    input_per_million_micros_usd: u64,
    output_per_million_micros_usd: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    cache_read_per_million_micros_usd: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    cache_write_per_million_micros_usd: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    reasoning_per_million_micros_usd: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(
    remote = "UsageAccounting",
    rename_all = "snake_case",
    tag = "kind",
    deny_unknown_fields
)]
enum AccountingFields {
    ApiDollars,
    UnpricedApi,
    SubscriptionQuota,
    AiCredits { micros_usd_per_credit: u64 },
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ProviderModelMetadata", deny_unknown_fields)]
struct MetadataFields {
    #[serde(with = "CapabilityFields")]
    capabilities: Capabilities,
    #[serde(with = "optional_pricing")]
    pricing: Option<ModelPricing>,
    #[serde(with = "AccountingFields")]
    accounting: UsageAccounting,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ProviderError", deny_unknown_fields)]
pub(super) struct ErrorFields {
    kind: crate::ProviderErrorKind,
    message: String,
    #[serde(deserialize_with = "Option::deserialize")]
    retry_after_ms: Option<u64>,
}

// These storage projections construct the source DTO directly. Adding a field
// to a DTO requires updating its recording projection; no map-based validator or
// external-provider decoding behavior is shared with this persistence boundary.
macro_rules! optional_projection {
    ($module:ident, $ty:ty, $fields:literal) => {
        pub(super) mod $module {
            use serde::{Deserialize, Deserializer, Serialize, Serializer};
            #[derive(Deserialize)]
            struct Owned(#[serde(with = $fields)] $ty);
            #[derive(Serialize)]
            struct Borrowed<'a>(#[serde(with = $fields)] &'a $ty);
            // Serde field serializers receive a reference to the complete field.
            #[allow(clippy::ref_option)]
            pub(in super::super) fn serialize<S: Serializer>(
                value: &Option<$ty>,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                match value {
                    Some(value) => serializer.serialize_some(&Borrowed(value)),
                    None => serializer.serialize_none(),
                }
            }
            pub(in super::super) fn deserialize<'de, D: Deserializer<'de>>(
                decoder: D,
            ) -> Result<Option<$ty>, D::Error> {
                Option::<Owned>::deserialize(decoder).map(|value| value.map(|value| value.0))
            }
        }
    };
}
optional_projection!(
    optional_metadata,
    crate::ProviderModelMetadata,
    "super::MetadataFields"
);
optional_projection!(
    optional_pricing,
    crate::ModelPricing,
    "super::PricingFields"
);
optional_projection!(optional_error, crate::ProviderError, "super::ErrorFields");
