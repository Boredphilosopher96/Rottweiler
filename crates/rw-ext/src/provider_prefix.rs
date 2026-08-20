use thiserror::Error;

/// Maximum wire length of an extension provider alias prefix, including `/`.
pub const MAX_PROVIDER_ALIAS_PREFIX_BYTES: usize = crate::plugin::MAX_NAME_BYTES;

/// A provider alias prefix did not satisfy the extension protocol contract.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "provider alias prefix must be 2-{MAX_PROVIDER_ALIAS_PREFIX_BYTES} ASCII bytes, end in '/', and contain only lowercase letters, digits, '-', '_', or '.' before '/'"
)]
pub struct ProviderAliasPrefixError;

/// Validates the canonical alias prefix shared by plugin manifests and provider composition.
///
/// # Errors
///
/// Returns [`ProviderAliasPrefixError`] when `prefix` is empty, too long, lacks its trailing
/// slash, or contains characters outside the extension protocol's canonical alphabet.
pub fn validate_provider_alias_prefix(prefix: &str) -> Result<(), ProviderAliasPrefixError> {
    let stem = prefix
        .strip_suffix('/')
        .filter(|stem| !stem.is_empty())
        .ok_or(ProviderAliasPrefixError)?;
    if prefix.len() > MAX_PROVIDER_ALIAS_PREFIX_BYTES
        || !stem.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(ProviderAliasPrefixError);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_alias_prefix_boundaries_are_canonical() {
        assert_eq!(validate_provider_alias_prefix("a/"), Ok(()));
        assert_eq!(
            validate_provider_alias_prefix(&format!(
                "{}/",
                "a".repeat(MAX_PROVIDER_ALIAS_PREFIX_BYTES - 1)
            )),
            Ok(())
        );
        for invalid in [
            "/".to_owned(),
            "a".repeat(MAX_PROVIDER_ALIAS_PREFIX_BYTES),
            "Upper/".to_owned(),
            "missing".to_owned(),
            "bad+prefix/".to_owned(),
            "nonascii-é/".to_owned(),
        ] {
            assert_eq!(
                validate_provider_alias_prefix(&invalid),
                Err(ProviderAliasPrefixError),
                "unexpectedly accepted {invalid:?}"
            );
        }
    }
}
