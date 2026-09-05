//! Bounded presentation of compatible subscription quota quantities.
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_QUOTA_QUANTITY_BYTES: usize = 128;
pub const MAX_QUOTA_UNIT_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionQuotaSummary {
    #[schemars(length(min = 1, max = MAX_QUOTA_QUANTITY_BYTES), regex(pattern = r"^[0-9]+(?:\.[0-9]+)?$"))]
    pub used: String,
    #[schemars(length(min = 1, max = MAX_QUOTA_UNIT_BYTES), extend("x-rw-max-utf8-bytes" = MAX_QUOTA_UNIT_BYTES))]
    pub unit: String,
}
impl<'de> Deserialize<'de> for SubscriptionQuotaSummary {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            used: String,
            unit: String,
        }
        let fields = Fields::deserialize(deserializer)?;
        let mut parts = fields.used.split('.');
        let integer = parts.next().unwrap_or_default();
        let fraction = parts.next();
        if fields.used.len() > MAX_QUOTA_QUANTITY_BYTES
            || integer.is_empty()
            || !integer.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|part| {
                part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit())
            })
            || parts.next().is_some()
            || fields.unit.is_empty()
            || fields.unit.len() > MAX_QUOTA_UNIT_BYTES
        {
            return Err(serde::de::Error::custom(
                "invalid subscription quota summary",
            ));
        }
        Ok(Self {
            used: fields.used,
            unit: fields.unit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_QUOTA_UNIT_BYTES, SubscriptionQuotaSummary};
    #[test]
    fn quota_summary_rejects_invalid_quantity_and_excessive_utf8_unit() {
        for used in ["", "-1", "1e9", "1.", "NaN"] {
            assert!(
                serde_json::from_value::<SubscriptionQuotaSummary>(
                    serde_json::json!({"used":used,"unit":"tokens"})
                )
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<SubscriptionQuotaSummary>(
                serde_json::json!({"used":"1","unit":"🙂".repeat(MAX_QUOTA_UNIT_BYTES / 4 + 1)})
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<SubscriptionQuotaSummary>(
                r#"{"used":"9007199254740993.1","unit":"requests"}"#
            )
            .is_ok()
        );
    }
}
