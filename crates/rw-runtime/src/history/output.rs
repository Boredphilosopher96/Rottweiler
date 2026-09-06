//! Retained export bytes are admitted before each append, including escaping.
use rw_types::json_encoding::JsonWriter;
use std::{
    fmt,
    io::{self, Write as _},
};

pub(super) struct Output {
    bytes: Vec<u8>,
    limit: usize,
}
impl Output {
    pub(super) const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
    pub(super) fn push(&mut self, text: &str) -> io::Result<()> {
        self.writer()?.write_all(text.as_bytes())
    }
    pub(super) fn writer(&mut self) -> io::Result<JsonWriter<'_>> {
        JsonWriter::buffer(&mut self.bytes, self.limit, 0)
    }
    pub(super) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    pub(super) fn ends_in_newline(&self) -> bool {
        self.bytes.last() == Some(&b'\n')
    }
    pub(super) fn pop_newline(&mut self) {
        if self.ends_in_newline() {
            self.bytes.pop();
        }
    }
    pub(super) fn finish(self) -> Vec<u8> {
        self.bytes
    }
    pub(super) fn text(self) -> io::Result<String> {
        String::from_utf8(self.bytes).map_err(io::Error::other)
    }
    pub(super) fn html(&mut self, value: &str) -> io::Result<()> {
        self.escaped(value, false)
    }
    pub(super) fn markdown(&mut self, value: &str) -> io::Result<()> {
        self.escaped(value, true)
    }
    fn escaped(&mut self, value: &str, markdown: bool) -> io::Result<()> {
        let mut start = 0;
        for (index, ch) in value.char_indices() {
            let escaped = match (markdown, ch) {
                (false, '&') => "&amp;",
                (_, '<') => "&lt;",
                (_, '>') => "&gt;",
                (false, '"') => "&quot;",
                (false, '\'') => "&#39;",
                (true, '\\') => "\\\\",
                (true, '*') => "\\*",
                (true, '_') => "\\_",
                (true, '[') => "\\[",
                (true, ']') => "\\]",
                (true, '\r' | '\n') => " ",
                _ => continue,
            };
            self.push(&value[start..index])?;
            self.push(escaped)?;
            start = index + ch.len_utf8();
        }
        self.push(&value[start..])
    }
}
impl fmt::Write for Output {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push(text).map_err(|_| fmt::Error)
    }
}
