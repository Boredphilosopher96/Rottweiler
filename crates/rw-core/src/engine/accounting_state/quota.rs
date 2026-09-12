use rw_types::billing::{MAX_QUOTA_QUANTITY_BYTES, MAX_QUOTA_UNIT_BYTES, SubscriptionQuotaSummary};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(super) enum Quota {
    #[default]
    Empty,
    Known(SubscriptionQuotaSummary),
    Unavailable,
}
impl Quota {
    pub(super) fn retained_heap_bytes(&self) -> Option<usize> {
        match self {
            Self::Known(value) => value.used.capacity().checked_add(value.unit.capacity()),
            Self::Empty | Self::Unavailable => Some(0),
        }
    }
    pub(super) fn add(&mut self, used: &str, unit: Option<&str>) {
        let unit = unit.unwrap_or("quota");
        if unit.is_empty()
            || unit.len() > MAX_QUOTA_UNIT_BYTES
            || unit.chars().any(char::is_control)
        {
            *self = Self::Unavailable;
            return;
        }
        let next = match self {
            Self::Empty => sum("0", used),
            Self::Known(value) if value.unit == unit => sum(&value.used, used),
            Self::Known(_) | Self::Unavailable => None,
        };
        *self = next.map_or(Self::Unavailable, |used| {
            Self::Known(SubscriptionQuotaSummary {
                used,
                unit: unit.to_owned(),
            })
        });
    }
    pub(super) fn summary(&self) -> Option<SubscriptionQuotaSummary> {
        match self {
            Self::Known(value) => Some(value.clone()),
            Self::Empty | Self::Unavailable => None,
        }
    }
}
fn digits(value: &str) -> Option<(Vec<u8>, usize)> {
    if value.is_empty() || value.len() > MAX_QUOTA_QUANTITY_BYTES {
        return None;
    }
    let mut parts = value.split('.');
    let integer = parts.next()?;
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || integer.is_empty()
        || !integer
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
        || value.ends_with('.')
    {
        return None;
    }
    Some((
        integer
            .bytes()
            .chain(fraction.bytes())
            .map(|byte| byte - b'0')
            .collect(),
        fraction.len(),
    ))
}
fn sum(left: &str, right: &str) -> Option<String> {
    let (mut left, left_scale) = digits(left)?;
    let (mut right, right_scale) = digits(right)?;
    let scale = left_scale.max(right_scale);
    left.resize(left.len() + scale - left_scale, 0);
    right.resize(right.len() + scale - right_scale, 0);
    let width = left.len().max(right.len());
    if width > MAX_QUOTA_QUANTITY_BYTES {
        return None;
    }
    let mut result = Vec::with_capacity(width + 1);
    let mut carry = 0;
    for index in 0..width {
        let l = left.len().checked_sub(index + 1).map_or(0, |i| left[i]);
        let r = right.len().checked_sub(index + 1).map_or(0, |i| right[i]);
        let total = l + r + carry;
        result.push(b'0' + total % 10);
        carry = total / 10;
    }
    if carry != 0 {
        result.push(b'0' + carry);
    }
    result.reverse();
    let mut result = String::from_utf8(result).ok()?;
    if scale != 0 {
        result.insert(result.len() - scale, '.');
    }
    let result = result.trim_start_matches('0');
    let mut result = if result.starts_with('.') {
        format!("0{result}")
    } else {
        result.to_owned()
    };
    if result.contains('.') {
        while result.ends_with('0') {
            result.pop();
        }
        if result.ends_with('.') {
            result.pop();
        }
    }
    if result.is_empty() {
        result.push('0');
    }
    (result.len() <= MAX_QUOTA_QUANTITY_BYTES).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::{Quota, sum};
    #[test]
    fn quota_addition_preserves_fractional_and_large_integer_precision() {
        assert_eq!(
            sum("9007199254740993.1", "0.2").as_deref(),
            Some("9007199254740993.3")
        );
        assert_eq!(sum("000.00", "0.000").as_deref(), Some("0"));
        assert_eq!(sum("99.999", "0.001").as_deref(), Some("100"));
        assert_eq!(sum("1", "-1"), None);
    }
    #[test]
    fn mixed_or_unrepresentable_quantities_remain_explicitly_unavailable() {
        let mut quota = Quota::default();
        quota.add("1", Some("requests"));
        quota.add("2", Some("tokens"));
        quota.add("3", Some("requests"));
        assert!(quota.summary().is_none());
        let mut quota = Quota::default();
        quota.add(&"1".repeat(129), None);
        assert!(quota.summary().is_none());
    }
}
