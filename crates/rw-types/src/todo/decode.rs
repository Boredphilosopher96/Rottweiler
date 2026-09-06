//! Reject task collection/aggregate overflow while decoding, before retaining another row.
use super::{
    MAX_TODO_CONTENT_BYTES, MAX_TODO_ID_BYTES, MAX_TODO_ITEMS, MAX_TODO_TOTAL_BYTES, TodoItem,
    TodoSnapshot, TodoStatus,
};
use serde::{
    Deserialize, Deserializer,
    de::{Error as _, SeqAccess, Visitor},
};
use std::{collections::BTreeSet, fmt};

impl<'de> Deserialize<'de> for TodoItem {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            id: String,
            content: String,
            status: TodoStatus,
        }
        let value = Fields::deserialize(decoder)?;
        if value.id.trim().is_empty()
            || value.content.trim().is_empty()
            || value.id.len() > MAX_TODO_ID_BYTES
            || value.content.len() > MAX_TODO_CONTENT_BYTES
        {
            return Err(D::Error::custom("task item exceeds identity/text bounds"));
        }
        Ok(Self {
            id: value.id,
            content: value.content,
            status: value.status,
        })
    }
}
impl<'de> Deserialize<'de> for TodoSnapshot {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(deserialize_with = "items")]
            items: Vec<TodoItem>,
        }
        let value = Fields::deserialize(decoder)?;
        Ok(Self { items: value.items })
    }
}
fn items<'de, D: Deserializer<'de>>(decoder: D) -> Result<Vec<TodoItem>, D::Error> {
    struct BoundedItems;
    impl<'de> Visitor<'de> for BoundedItems {
        type Value = Vec<TodoItem>;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a bounded task list")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut items = Vec::<TodoItem>::new();
            let mut ids = BTreeSet::new();
            let mut bytes = 0;
            loop {
                if items.len() == MAX_TODO_ITEMS {
                    if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                        return Err(A::Error::custom("task item count exceeds limit"));
                    }
                    break;
                }
                let Some(item) = sequence.next_element::<TodoItem>()? else {
                    break;
                };
                if !ids.insert(item.id.clone()) {
                    return Err(A::Error::custom("duplicate task identity"));
                }
                bytes += item.id.len() + item.content.len();
                if bytes > MAX_TODO_TOTAL_BYTES {
                    return Err(A::Error::custom("task aggregate text exceeds limit"));
                }
                items.push(item);
            }
            Ok(items)
        }
    }
    decoder.deserialize_seq(BoundedItems)
}

#[cfg(test)]
mod tests {
    use super::super::{MAX_TODO_ITEMS, TodoSnapshot};
    #[test]
    fn snapshot_decode_rejects_duplicate_and_aggregate_overflow() {
        let item = serde_json::json!({"id":"task","content":"a","status":"pending"});
        assert!(
            serde_json::from_value::<TodoSnapshot>(
                serde_json::json!({"items":[item.clone(),item]})
            )
            .is_err()
        );
        let items = (0..20).map(|i| serde_json::json!({"id":i.to_string(),"content":"x".repeat(4096),"status":"pending"})).collect::<Vec<_>>();
        assert!(
            serde_json::from_value::<TodoSnapshot>(serde_json::json!({"items":items})).is_err()
        );
        let items = (0..=MAX_TODO_ITEMS)
            .map(|i| serde_json::json!({"id":i.to_string(),"content":"x","status":"pending"}))
            .collect::<Vec<_>>();
        assert!(
            serde_json::from_value::<TodoSnapshot>(serde_json::json!({"items":items})).is_err()
        );
    }
}
