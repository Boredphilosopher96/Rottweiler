/// Shared redaction hook applied before tool text enters persistence,
/// broadcast, or the next provider request.
pub trait SecretRedactor: Send + Sync {
    fn redact(&self, text: &str) -> String;

    /// Longest secret that may be replaced, so streaming boundaries can retain
    /// enough overlap to avoid exposing a value split across provider chunks.
    fn max_secret_bytes(&self) -> usize {
        0
    }

    /// Returns true while `text` ends inside a strict secret envelope whose
    /// terminator has not arrived yet. Streaming callers retain the whole
    /// pending envelope rather than relying on a fixed overlap.
    fn has_incomplete_secret_envelope(&self, _text: &str) -> bool {
        false
    }
}

#[derive(Debug, Default)]
pub struct NoopSecretRedactor;

impl SecretRedactor for NoopSecretRedactor {
    fn redact(&self, text: &str) -> String {
        text.to_owned()
    }
}

pub(super) struct StreamingSecretRedactor<'a> {
    pub(super) redactor: &'a dyn SecretRedactor,
    pub(super) raw: String,
    pub(super) emitted: String,
    pub(super) overlap_bytes: usize,
}

impl<'a> StreamingSecretRedactor<'a> {
    pub(super) fn new(redactor: &'a dyn SecretRedactor) -> Self {
        Self {
            redactor,
            raw: String::new(),
            emitted: String::new(),
            overlap_bytes: redactor.max_secret_bytes().saturating_sub(1),
        }
    }

    pub(super) fn push(&mut self, chunk: &str) -> String {
        self.raw.push_str(chunk);
        if self.redactor.has_incomplete_secret_envelope(&self.raw) {
            return String::new();
        }
        let redacted = self.redactor.redact(&self.raw);
        if redacted.len() <= self.overlap_bytes || !redacted.starts_with(&self.emitted) {
            return String::new();
        }
        let mut safe_end = redacted.len().saturating_sub(self.overlap_bytes);
        while safe_end > self.emitted.len() && !redacted.is_char_boundary(safe_end) {
            safe_end = safe_end.saturating_sub(1);
        }
        if safe_end <= self.emitted.len() {
            return String::new();
        }
        let delta = redacted[self.emitted.len()..safe_end].to_owned();
        self.emitted.push_str(&delta);
        delta
    }

    pub(super) fn finish(&mut self) -> String {
        let redacted = if self.redactor.has_incomplete_secret_envelope(&self.raw) {
            "[REDACTED]".to_owned()
        } else {
            self.redactor.redact(&self.raw)
        };
        let delta = redacted
            .strip_prefix(&self.emitted)
            .unwrap_or(&redacted)
            .to_owned();
        self.raw.clear();
        self.emitted.clear();
        delta
    }
}
