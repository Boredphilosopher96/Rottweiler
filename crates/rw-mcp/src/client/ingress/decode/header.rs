//! Direct scalar visitors avoid rmcp's generic Value allocation for RPC IDs.
use super::{invalid, json_error};
use rmcp::model::RequestId;
use serde::{
    Deserializer as _,
    de::{self, Visitor},
};
use std::{fmt, io, sync::Arc};

pub(super) fn id(raw: &str) -> io::Result<RequestId> {
    struct Id;
    impl Visitor<'_> for Id {
        type Value = RequestId;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an integer or string RPC ID")
        }
        fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
            Ok(RequestId::Number(value))
        }
        fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
            i64::try_from(value)
                .map(RequestId::Number)
                .map_err(|_| E::custom("RPC ID exceeds signed integer range"))
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok(RequestId::String(Arc::from(value)))
        }
    }
    let mut decoder = serde_json::Deserializer::from_str(raw);
    let result = decoder.deserialize_any(Id).map_err(json_error)?;
    decoder.end().map_err(json_error)?;
    Ok(result)
}

pub(super) fn method(raw: &str) -> io::Result<String> {
    struct Method;
    impl Visitor<'_> for Method {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an admitted RPC method")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            if value.is_empty() || value.len() > 256 {
                return Err(E::custom("MCP method exceeds routing admission"));
            }
            Ok(value.to_owned())
        }
    }
    serde_json::Deserializer::from_str(raw)
        .deserialize_str(Method)
        .map_err(json_error)
}

pub(super) fn version(raw: &str) -> io::Result<()> {
    struct Version;
    impl Visitor<'_> for Version {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("JSON-RPC version 2.0")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<bool, E> {
            Ok(value == "2.0")
        }
    }
    if serde_json::Deserializer::from_str(raw)
        .deserialize_str(Version)
        .map_err(json_error)?
    {
        Ok(())
    } else {
        Err(invalid("MCP JSON-RPC version must be 2.0"))
    }
}

pub(super) fn numeric_id(raw: &str) -> io::Result<Option<i64>> {
    struct Numeric;
    impl Visitor<'_> for Numeric {
        type Value = Option<i64>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an integer or string RPC ID")
        }
        fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
            Ok(Some(value))
        }
        fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
            i64::try_from(value)
                .map(Some)
                .map_err(|_| E::custom("RPC ID exceeds signed integer range"))
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok(value.parse().ok())
        }
    }
    serde_json::Deserializer::from_str(raw)
        .deserialize_any(Numeric)
        .map_err(json_error)
}

pub(super) fn matches_id(raw: &str, request: &RequestId) -> io::Result<bool> {
    struct Matches<'a>(&'a RequestId);
    impl Visitor<'_> for Matches<'_> {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an integer or string RPC ID")
        }
        fn visit_i64<E: de::Error>(self, value: i64) -> Result<bool, E> {
            Ok(matches!(self.0, RequestId::Number(expected) if *expected == value))
        }
        fn visit_u64<E: de::Error>(self, value: u64) -> Result<bool, E> {
            self.visit_i64(
                i64::try_from(value)
                    .map_err(|_| E::custom("RPC ID exceeds signed integer range"))?,
            )
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<bool, E> {
            Ok(match self.0 {
                RequestId::String(expected) => value == expected.as_ref(),
                RequestId::Number(expected) => value.parse::<i64>() == Ok(*expected),
            })
        }
    }
    serde_json::Deserializer::from_str(raw)
        .deserialize_any(Matches(request))
        .map_err(json_error)
}
