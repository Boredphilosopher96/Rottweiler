//! Validate borrowed JSON without constructing a second document graph.
use super::{
    MAX_OUTPUT_SCHEMA_DEPTH, MAX_STRUCTURED_OUTPUT_BYTES, MAX_STRUCTURED_OUTPUT_NODES, OutputField,
    OutputSchema,
};
use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use std::fmt;

pub(super) fn validate(schema: &OutputSchema, text: &str) -> Result<(), serde_json::Error> {
    if text.len() > MAX_STRUCTURED_OUTPUT_BYTES {
        return Err(de::Error::custom("structured output byte limit"));
    }
    let mut nodes = 0;
    let mut decoder = serde_json::Deserializer::from_str(text);
    Seed {
        schema,
        nodes: &mut nodes,
        depth: 1,
    }
    .deserialize(&mut decoder)?;
    decoder.end()
}

struct Seed<'a> {
    schema: &'a OutputSchema,
    nodes: &'a mut usize,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        *self.nodes += 1;
        if *self.nodes > MAX_STRUCTURED_OUTPUT_NODES || self.depth > MAX_OUTPUT_SCHEMA_DEPTH {
            return Err(de::Error::custom("structured output node or depth limit"));
        }
        if matches!(self.nonnull(), OutputSchema::Integer {}) {
            let raw = <&serde_json::value::RawValue>::deserialize(decoder)?;
            if (raw.get() == "null" && matches!(self.schema, OutputSchema::Nullable { .. }))
                || exact_integer(raw.get())
            {
                return Ok(());
            }
            return Err(de::Error::custom("unexpected non-integer number"));
        }
        decoder.deserialize_any(self)
    }
}
impl Seed<'_> {
    fn nonnull(&self) -> &OutputSchema {
        let mut schema = self.schema;
        while let OutputSchema::Nullable { value } = schema {
            schema = value;
        }
        schema
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a value matching the structured output schema")
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        if matches!(
            self.schema,
            OutputSchema::Null {} | OutputSchema::Nullable { .. }
        ) {
            Ok(())
        } else {
            Err(E::custom("unexpected null"))
        }
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        if matches!(self.nonnull(), OutputSchema::Boolean {}) {
            Ok(())
        } else {
            Err(E::custom("unexpected boolean"))
        }
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        if matches!(self.nonnull(), OutputSchema::String {}) {
            Ok(())
        } else {
            Err(E::custom("unexpected string"))
        }
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        self.integer()
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        self.integer()
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<(), E> {
        if value.is_finite() && matches!(self.nonnull(), OutputSchema::Number {}) {
            Ok(())
        } else {
            Err(E::custom("unexpected number"))
        }
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        let mut schema = self.schema;
        while let OutputSchema::Nullable { value } = schema {
            schema = value;
        }
        let OutputSchema::Array { items } = schema else {
            return Err(de::Error::custom("unexpected array"));
        };
        while sequence
            .next_element_seed(Seed {
                schema: items,
                nodes: self.nodes,
                depth: self.depth + 1,
            })?
            .is_some()
        {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut schema = self.schema;
        while let OutputSchema::Nullable { value } = schema {
            schema = value;
        }
        let OutputSchema::Object { fields } = schema else {
            return Err(de::Error::custom("unexpected object"));
        };
        let mut seen = 0_u64;
        while let Some(index) = map.next_key_seed(Key(fields))? {
            let bit = 1_u64 << index;
            if seen & bit != 0 {
                return Err(de::Error::custom("duplicate structured field"));
            }
            seen |= bit;
            *self.nodes += 1;
            map.next_value_seed(Seed {
                schema: &fields[index].schema,
                nodes: self.nodes,
                depth: self.depth + 1,
            })?;
        }
        let required = if fields.len() == 64 {
            u64::MAX
        } else {
            (1_u64 << fields.len()) - 1
        };
        if seen != required {
            return Err(de::Error::custom("missing structured field"));
        }
        Ok(())
    }
}
impl Seed<'_> {
    fn integer<E: de::Error>(&self) -> Result<(), E> {
        if matches!(
            self.nonnull(),
            OutputSchema::Integer {} | OutputSchema::Number {}
        ) {
            Ok(())
        } else {
            Err(E::custom("unexpected integer"))
        }
    }
}
struct Key<'a>(&'a [OutputField]);
impl<'de> DeserializeSeed<'de> for Key<'_> {
    type Value = usize;
    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<usize, D::Error> {
        decoder.deserialize_str(self)
    }
}
impl Visitor<'_> for Key<'_> {
    type Value = usize;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a declared structured field")
    }
    fn visit_str<E: de::Error>(self, name: &str) -> Result<usize, E> {
        self.0
            .iter()
            .position(|field| field.name == name)
            .ok_or_else(|| E::custom("unknown structured field"))
    }
}

// Inspect decimal significance, not a rounded f64's fractional part. 1.000...01
// must not become an integer merely because IEEE-754 rounds it to one.
fn exact_integer(raw: &str) -> bool {
    let Ok(value) = raw.parse::<f64>() else {
        return false;
    };
    if !value.is_finite() {
        return false;
    }
    let raw = raw.strip_prefix('-').unwrap_or(raw);
    let (mantissa, exponent) = raw.split_once(['e', 'E']).unwrap_or((raw, "0"));
    let Ok(exponent) = exponent.parse::<i32>() else {
        return false;
    };
    if mantissa.bytes().all(|byte| byte == b'0' || byte == b'.') {
        return true;
    }
    let fractional = mantissa.split_once('.').map_or(0, |(_, tail)| tail.len());
    let zeros = mantissa
        .bytes()
        .rev()
        .filter(|byte| *byte != b'.')
        .take_while(|byte| *byte == b'0')
        .count();
    i64::from(exponent) - i64::try_from(fractional).unwrap_or(i64::MAX)
        >= -i64::try_from(zeros).unwrap_or(i64::MAX)
}
