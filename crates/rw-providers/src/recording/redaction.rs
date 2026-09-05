use super::Arc;

/// Shared known-secret redactor applied before fixture bytes reach disk.
///
/// Clones share one registry so credentials learned after provider composition
/// (for example, refreshed OAuth tokens) are visible to an already-created
/// recorder before it serializes a response.
#[derive(Clone, Default)]
pub struct FixtureRedactor {
    pub(super) secrets: Arc<std::sync::RwLock<Vec<String>>>,
}

impl std::fmt::Debug for FixtureRedactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FixtureRedactor")
            .field("registered_secret_count", &self.registered_secret_count())
            .finish_non_exhaustive()
    }
}

impl FixtureRedactor {
    /// Creates a redactor from registered secrets. Empty values are ignored.
    #[must_use]
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let redactor = Self::default();
        for secret in secrets {
            redactor.register_value(secret);
        }
        redactor
    }

    /// Registers a credential without exposing it through the type system.
    /// Empty values are ignored and duplicate registrations are deduplicated.
    pub fn register_secret(&self, secret: &crate::Secret) {
        self.register_value(secret.expose_secret().to_owned());
    }

    /// Registers a value already classified as sensitive by a trusted
    /// composition boundary, such as an environment variable whose name ends
    /// in `_API_KEY`. The value is never exposed by this type.
    pub fn register_known_value(&self, value: &str) {
        self.register_value(value.to_owned());
    }

    /// Merges another trusted registry without exposing credential values.
    /// Providers composed after in-app authentication use this to extend the
    /// already-running engine's redaction boundary.
    pub fn merge_from(&self, other: &Self) {
        let values = match other.secrets.read() {
            Ok(secrets) => secrets.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        for value in values {
            self.register_value(value);
        }
    }

    /// Number of non-empty known secrets registered for fixture sanitization.
    /// This exposes no credential material and supports acceptance assertions
    /// that every preflighted credential reached the recording boundary.
    #[must_use]
    pub fn registered_secret_count(&self) -> usize {
        match self.secrets.read() {
            Ok(secrets) => secrets.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Longest registered secret, used by bounded streaming redactors to keep
    /// exactly enough overlap between arbitrary transport chunks.
    #[must_use]
    pub fn maximum_registered_secret_bytes(&self) -> usize {
        match self.secrets.read() {
            Ok(secrets) => secrets.iter().map(String::len).max().unwrap_or(0),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .map(String::len)
                .max()
                .unwrap_or(0),
        }
    }

    /// Whether already-rendered content still contains a registered secret.
    /// The result exposes no secret value.
    #[must_use]
    pub fn contains_registered_secret(&self, value: &str) -> bool {
        match self.secrets.read() {
            Ok(secrets) => secrets.iter().any(|secret| value.contains(secret)),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .any(|secret| value.contains(secret)),
        }
    }

    /// Replaces every registered known secret in arbitrary fixture text.
    ///
    /// This lets non-provider fixture recorders share the same live secret
    /// registry before their bytes reach disk.
    #[must_use]
    pub fn redact_text(&self, value: &str) -> String {
        self.redact(value)
    }

    /// Redacts strings while bounding each owned replacement allocation.
    /// The original string remains with its caller. At most two `max_bytes`
    /// intermediate strings coexist; replacement markers follow the same ordered
    /// exact-secret and strict-format rules as `redact_text`.
    ///
    /// # Errors
    /// Rejects input or any replacement stage exceeding the caller's byte budget.
    pub fn redact_text_bounded(&self, value: &str, max_bytes: usize) -> std::io::Result<String> {
        if value.len() > max_bytes {
            return Err(redaction_limit());
        }
        let secrets = self
            .secrets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rendered = value.to_owned();
        for secret in secrets.iter() {
            if let Some(replaced) = limited_replacement(&rendered, max_bytes, || {
                rendered
                    .match_indices(secret.as_str())
                    .map(|(start, value)| (start, start + value.len()))
            })? {
                rendered = replaced;
            }
        }
        for pattern in strict_patterns() {
            if let Some(replaced) = limited_replacement(&rendered, max_bytes, || {
                pattern
                    .find_iter(&rendered)
                    .map(|found| (found.start(), found.end()))
            })? {
                rendered = replaced;
            }
        }
        Ok(rendered)
    }

    /// Redacts exact registered credential bytes before a transport chunk is
    /// encoded for an untrusted boundary. Callers retain cross-chunk overlap.
    #[must_use]
    pub fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        let redact_with = |secrets: &[String]| {
            let mut rendered = value.to_vec();
            for secret in secrets {
                rendered = replace_bytes(&rendered, secret.as_bytes(), b"[REDACTED]");
            }
            String::from_utf8(rendered.clone())
                .map(|text| redact_strict_key_formats(&text).into_bytes())
                .unwrap_or(rendered)
        };
        match self.secrets.read() {
            Ok(secrets) => redact_with(&secrets),
            Err(poisoned) => redact_with(&poisoned.into_inner()),
        }
    }

    /// Redacts the safely-emittable prefix of a streaming buffer while
    /// retaining enough original bytes to detect credentials split across the
    /// next transport chunk.
    #[must_use]
    pub fn redact_streaming_prefix(&self, value: &[u8], retain: usize) -> (Vec<u8>, Vec<u8>) {
        let initial_boundary = value.len().saturating_sub(retain);
        let extend_boundary = |secrets: &[String]| {
            let mut boundary = initial_boundary;
            loop {
                let extended = secrets.iter().fold(boundary, |extended, secret| {
                    if secret.is_empty() || secret.len() > value.len() {
                        return extended;
                    }
                    value
                        .windows(secret.len())
                        .enumerate()
                        .filter(|(start, window)| *start < extended && *window == secret.as_bytes())
                        .map(|(start, _)| start + secret.len())
                        .max()
                        .map_or(extended, |end| extended.max(end))
                });
                if extended == boundary {
                    return boundary;
                }
                boundary = extended.min(value.len());
            }
        };
        let boundary = match self.secrets.read() {
            Ok(secrets) => extend_boundary(&secrets),
            Err(poisoned) => extend_boundary(&poisoned.into_inner()),
        };
        (
            self.redact_bytes(&value[..boundary]),
            value[boundary..].to_vec(),
        )
    }

    pub(super) fn redact(&self, value: &str) -> String {
        let redact_with = |secrets: &[String]| {
            let rendered = secrets.iter().fold(value.to_owned(), |rendered, secret| {
                rendered.replace(secret, "[REDACTED]")
            });
            redact_strict_key_formats(&rendered)
        };
        match self.secrets.read() {
            Ok(secrets) => redact_with(&secrets),
            Err(poisoned) => redact_with(&poisoned.into_inner()),
        }
    }

    fn register_value(&self, secret: String) {
        if secret.is_empty() {
            return;
        }
        let mut secrets = match self.secrets.write() {
            Ok(secrets) => secrets,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !secrets.contains(&secret) {
            secrets.push(secret);
        }
    }
}

pub(super) fn replace_bytes(input: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return input.to_vec();
    }
    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0;
    while let Some(relative) = input[offset..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let found = offset + relative;
        output.extend_from_slice(&input[offset..found]);
        output.extend_from_slice(replacement);
        offset = found + needle.len();
    }
    output.extend_from_slice(&input[offset..]);
    output
}

/// Redacts credential formats with stable vendor-defined markers. Entropy
/// heuristics are deliberately excluded so ordinary hashes, base64 payloads,
/// and minified source remain intact at the model-context boundary.
pub(super) fn redact_strict_key_formats(value: &str) -> String {
    strict_patterns()
        .iter()
        .fold(value.to_owned(), |redacted, pattern| {
            pattern.replace_all(&redacted, "[REDACTED]").into_owned()
        })
}

fn strict_patterns() -> &'static [regex::Regex] {
    use std::sync::OnceLock;

    use regex::Regex;

    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // PEM private keys, including PKCS#1, PKCS#8, EC, and OpenSSH.
            r"(?s)-----BEGIN [^-\r\n]*PRIVATE KEY-----.*?-----END [^-\r\n]*PRIVATE KEY-----",
            // OpenAI/Anthropic-style secret keys.
            r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}\b",
            // GitHub personal, OAuth, user, server, and refresh tokens.
            r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
            // AWS access-key identifiers.
            r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
            // Google API keys, Slack tokens, and npm granular tokens.
            r"\bAIza[0-9A-Za-z_-]{35}\b",
            r"\bxox[baprs]-[0-9A-Za-z-]{20,}\b",
            r"\bnpm_[A-Za-z0-9]{36}\b",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).unwrap_or_else(|_| unreachable!("static regex")))
        .collect()
    })
}

fn redaction_limit() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "redacted string exceeds byte budget",
    )
}

fn limited_replacement<F, I>(
    value: &str,
    limit: usize,
    ranges: F,
) -> std::io::Result<Option<String>>
where
    F: Fn() -> I,
    I: Iterator<Item = (usize, usize)>,
{
    let mut any = false;
    let length = ranges()
        .try_fold(value.len(), |length, (start, end)| {
            any = true;
            length
                .checked_sub(end.checked_sub(start)?)?
                .checked_add("[REDACTED]".len())
        })
        .filter(|length| *length <= limit)
        .ok_or_else(redaction_limit)?;
    if !any {
        return Ok(None);
    }
    let mut output = String::with_capacity(length);
    let mut offset = 0;
    for (start, end) in ranges() {
        output.push_str(&value[offset..start]);
        output.push_str("[REDACTED]");
        offset = end;
    }
    output.push_str(&value[offset..]);
    Ok(Some(output))
}

impl crate::KnownSecretRegistrar for FixtureRedactor {
    fn register(&self, secret: &crate::Secret) {
        self.register_secret(secret);
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::FixtureRedactor;
    #[test]
    fn bounded_text_matches_ordered_semantics_and_rejects_expansion_before_allocating() {
        let redactor = FixtureRedactor::new(["x".to_owned(), "E".to_owned()]);
        assert!(
            redactor
                .redact_text_bounded(&"x".repeat(1024), 1024)
                .is_err()
        );
        let short = "hello x and E";
        assert_eq!(
            redactor.redact_text_bounded(short, 4096).ok(),
            Some(redactor.redact_text(short))
        );
        let key = "sk-012345678901234567890123456789";
        assert_eq!(
            FixtureRedactor::default()
                .redact_text_bounded(key, 1024)
                .ok(),
            Some("[REDACTED]".into())
        );
        assert!(
            FixtureRedactor::default()
                .redact_text_bounded("four", 3)
                .is_err()
        );
    }
}
