//! Source-qualified authority for bounded session-family reads.

use crate::{SequenceId, SessionId, SubagentId};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de};
use ts_rs::TS;

/// One query validates at most this many effective parent-child associations.
pub const MAX_SESSION_READ_ANCESTORS: usize = 8;
pub const MAX_SESSION_READ_SUBAGENT_ID_BYTES: usize = 128;

/// A hop is bound to its parent's canonical spawn source, never just a child name.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionReadAncestor {
    #[schemars(length(min = 1, max = MAX_SESSION_READ_SUBAGENT_ID_BYTES), extend("x-rw-max-utf8-bytes" = MAX_SESSION_READ_SUBAGENT_ID_BYTES))]
    pub subagent_id: SubagentId,
    pub session_id: SessionId,
    pub source_sequence: SequenceId,
}

/// Direct session authority or an explicit path from an independently authorized root.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionReadScope {
    Session {},
    Descendant {
        root_session_id: SessionId,
        #[serde(deserialize_with = "decode_ancestors")]
        #[schemars(length(min = 1, max = MAX_SESSION_READ_ANCESTORS))]
        ancestry: Vec<SessionReadAncestor>,
    },
}

impl SessionReadScope {
    /// Validate path shape and return the root requiring independent authorization.
    ///
    /// # Errors
    /// Rejects invalid identities, empty/deep paths, cycles and a mismatched target.
    pub fn root<'a>(&'a self, target: &'a SessionId) -> Result<&'a SessionId, &'static str> {
        SessionId::validate(&target.0).map_err(|_| "invalid read session")?;
        let Self::Descendant {
            root_session_id,
            ancestry,
        } = self
        else {
            return Ok(target);
        };
        SessionId::validate(&root_session_id.0).map_err(|_| "invalid read root")?;
        if ancestry.is_empty() || ancestry.len() > MAX_SESSION_READ_ANCESTORS {
            return Err("invalid read ancestry depth");
        }
        for (index, hop) in ancestry.iter().enumerate() {
            SessionId::validate(&hop.session_id.0).map_err(|_| "invalid child read session")?;
            if hop.subagent_id.0.is_empty()
                || hop.subagent_id.0.len() > MAX_SESSION_READ_SUBAGENT_ID_BYTES
            {
                return Err("invalid read subagent identity");
            }
            if hop.session_id == *root_session_id
                || ancestry[..index]
                    .iter()
                    .any(|prior| prior.session_id == hop.session_id)
            {
                return Err("cyclic read ancestry");
            }
        }
        if ancestry.last().is_none_or(|hop| hop.session_id != *target) {
            return Err("read ancestry target mismatch");
        }
        Ok(root_session_id)
    }
}

fn decode_ancestors<'de, D: de::Deserializer<'de>>(
    decoder: D,
) -> Result<Vec<SessionReadAncestor>, D::Error> {
    struct Ancestors;
    impl<'de> de::Visitor<'de> for Ancestors {
        type Value = Vec<SessionReadAncestor>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a nonempty bounded read ancestry")
        }
        fn visit_seq<A: de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = Vec::with_capacity(MAX_SESSION_READ_ANCESTORS);
            while let Some(value) = sequence.next_element()? {
                if values.len() == MAX_SESSION_READ_ANCESTORS {
                    return Err(de::Error::custom("read ancestry exceeds depth limit"));
                }
                values.push(value);
            }
            if values.is_empty() {
                return Err(de::Error::custom("read ancestry is empty"));
            }
            Ok(values)
        }
    }
    decoder.deserialize_seq(Ancestors)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SESSION_READ_ANCESTORS, SequenceId, SessionId, SessionReadAncestor, SessionReadScope,
        SubagentId,
    };
    #[test]
    fn direct_root_and_source_qualified_path_reject_wrong_target_and_cycles() {
        let target = SessionId("child".into());
        let mut scope = SessionReadScope::Descendant {
            root_session_id: SessionId("parent".into()),
            ancestry: vec![SessionReadAncestor {
                subagent_id: SubagentId("agent".into()),
                session_id: target.clone(),
                source_sequence: SequenceId(9),
            }],
        };
        assert_eq!(scope.root(&target), Ok(&SessionId("parent".into())));
        assert!(scope.root(&SessionId("foreign".into())).is_err());
        if let SessionReadScope::Descendant { ancestry, .. } = &mut scope {
            ancestry.push(ancestry[0].clone());
        }
        assert!(scope.root(&target).is_err());
        assert_eq!(SessionReadScope::Session {}.root(&target), Ok(&target));
    }
    #[test]
    fn wire_requires_closed_explicit_scope_and_bounds_ancestry_while_decoding() {
        assert!(
            serde_json::from_value::<SessionReadScope>(
                serde_json::json!({"type":"session","ancestry":[]})
            )
            .is_err()
        );
        let hop =
            serde_json::json!({"subagent_id":"agent","session_id":"child","source_sequence":"9"});
        for ancestry in [vec![], vec![hop; MAX_SESSION_READ_ANCESTORS + 1]] {
            assert!(serde_json::from_value::<SessionReadScope>(serde_json::json!({"type":"descendant","root_session_id":"parent","ancestry":ancestry})).is_err());
        }
        let schema = serde_json::to_value(schemars::schema_for!(SessionReadScope))
            .unwrap_or_else(|error| panic!("schema: {error}"));
        assert_eq!(
            schema["oneOf"][1]["properties"]["ancestry"]["maxItems"],
            MAX_SESSION_READ_ANCESTORS
        );
    }
}
