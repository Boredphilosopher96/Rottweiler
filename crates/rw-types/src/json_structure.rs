//! Borrowed JSON admission before allocating typed values or generic containers.
use crate::allocation::DecodeAllocation;
use serde::{
    Deserializer,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use std::fmt;

#[derive(Clone, Copy, Debug)]
pub struct JsonStructureLimits {
    pub max_encoded_bytes: usize,
    pub max_nodes: usize,
    pub max_string_bytes: usize,
    pub max_depth: usize,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JsonStructure {
    pub nodes: usize,
    pub string_bytes: usize,
    pub depth: usize,
    pub objects: usize,
    pub object_entries: usize,
    pub arrays: usize,
    pub array_entries: usize,
}
impl JsonStructure {
    /// Bound direct `serde_json::Value` decoding without a tagged Content tree.
    /// Objects charge their own map backing; scalar leaves do not each own a map.
    /// Counts include spare vector capacity and simultaneous decoder/string growth.
    /// The encoded input buffer remains separately owned by the caller.
    #[must_use]
    pub fn direct_value_decode_bytes(&self) -> Option<usize> {
        let maps = self
            .objects
            .checked_add(self.object_entries)?
            .checked_mul(<serde_json::Value as DecodeAllocation>::decode_node_bytes()?)?;
        let vectors = self
            .array_entries
            .checked_mul(std::mem::size_of::<serde_json::Value>())?
            .checked_mul(2)?
            .checked_add(self.arrays.checked_mul(128)?)?;
        self.nodes
            .checked_mul(std::mem::size_of::<serde_json::Value>())?
            .checked_add(maps)?
            .checked_add(vectors)?
            .checked_add(self.string_bytes)?
            .checked_mul(2)
    }

    /// Charge typed containers, owned serde intermediates, string growth and scratch.
    /// The caller separately owns the encoded input buffer.
    #[must_use]
    pub fn decode_bytes<T: DecodeAllocation>(&self) -> Option<usize> {
        // Four simultaneous representations cover typed capacity growth plus the
        // owned Content tree used by tagged derives and serde's decoding scratch.
        // No untagged trial decoders or hidden payload allocations are permitted
        // by DecodeAllocation's contract.
        let nodes = self.nodes.checked_mul(T::decode_node_bytes()?.max(128))?;
        nodes.checked_add(self.string_bytes)?.checked_mul(4)
    }
}

/// Visit JSON without constructing strings, arrays, maps or typed wire values.
/// Escaped strings use serde's one reusable scratch buffer, bounded by input size.
///
/// # Errors
/// Rejects malformed JSON, trailing data and any structural admission overflow.
pub fn preflight_json(
    input: &[u8],
    limits: JsonStructureLimits,
) -> Result<JsonStructure, serde_json::Error> {
    if input.len() > limits.max_encoded_bytes {
        return Err(<serde_json::Error as de::Error>::custom(
            "JSON encoded admission exceeded",
        ));
    }
    let mut shape = JsonStructure::default();
    let mut decoder = serde_json::Deserializer::from_slice(input);
    Seed {
        shape: &mut shape,
        limits,
        depth: 0,
    }
    .deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(shape)
}
struct Seed<'a> {
    shape: &'a mut JsonStructure,
    limits: JsonStructureLimits,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        self.shape.nodes = self
            .shape
            .nodes
            .checked_add(1)
            .ok_or_else(|| de::Error::custom("JSON node overflow"))?;
        if self.shape.nodes > self.limits.max_nodes || self.depth > self.limits.max_depth {
            return Err(de::Error::custom("JSON structural admission exceeded"));
        }
        self.shape.depth = self.shape.depth.max(self.depth);
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded JSON")
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<(), E> {
        self.shape.string_bytes = self
            .shape
            .string_bytes
            .checked_add(value.len())
            .ok_or_else(|| E::custom("JSON text overflow"))?;
        if self.shape.string_bytes > self.limits.max_string_bytes {
            return Err(E::custom("JSON text admission exceeded"));
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<(), A::Error> {
        self.shape.arrays += 1;
        while values
            .next_element_seed(Seed {
                shape: self.shape,
                limits: self.limits,
                depth: self.depth + 1,
            })?
            .is_some()
        {
            self.shape.array_entries += 1;
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut values: A) -> Result<(), A::Error> {
        self.shape.objects += 1;
        while values
            .next_key_seed(Seed {
                shape: self.shape,
                limits: self.limits,
                depth: self.depth + 1,
            })?
            .is_some()
        {
            self.shape.object_entries += 1;
            values.next_value_seed(Seed {
                shape: self.shape,
                limits: self.limits,
                depth: self.depth + 1,
            })?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests;
