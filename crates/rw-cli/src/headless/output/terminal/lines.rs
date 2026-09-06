//! Bounded UTF-8 line input; rich editing and history belong to the TUI.
use super::super::{InputLine, input::InputSender};
use std::io;

pub(super) const MAX_LINE_BYTES: usize = 128 * 1024;
pub(super) const MAX_ECHO_BYTES: usize = 2 * MAX_LINE_BYTES;

pub(super) struct Lines {
    bytes: Vec<u8>,
    interactive: bool,
    after_cr: bool,
    pub ended: bool,
}
impl Lines {
    pub fn new(interactive: bool) -> Self {
        Self {
            bytes: Vec::with_capacity(MAX_LINE_BYTES),
            interactive,
            after_cr: false,
            ended: false,
        }
    }
    pub fn push(
        &mut self,
        input: &[u8],
        sender: &InputSender,
        echo: &mut Vec<u8>,
        interrupts: &tokio::sync::watch::Sender<()>,
    ) -> io::Result<()> {
        for &byte in input {
            if self.ended {
                break;
            }
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = false;
            match byte {
                b'\r' | b'\n' => {
                    self.submit(sender)?;
                    if self.interactive {
                        append(echo, b"\r\nrw> ")?;
                    }
                    self.after_cr = byte == b'\r';
                }
                3 if self.interactive => {
                    self.bytes.clear();
                    interrupts.send_replace(());
                    append(echo, b"^C\r\nrw> ")?;
                }
                4 if self.interactive => self.eof(sender)?,
                8 | 127 if self.interactive => {
                    if self.erase() {
                        self.reprint(echo)?;
                    }
                }
                21 if self.interactive => {
                    self.bytes.clear();
                    self.reprint(echo)?;
                }
                _ => {
                    if self.interactive && byte < 32 && byte != b'\t' {
                        return Err(io::Error::other(
                            "headless REPL supports text, erase, Ctrl-U, Ctrl-C and Ctrl-D; use the TUI for rich editing",
                        ));
                    }
                    if self.bytes.len() == MAX_LINE_BYTES {
                        return Err(io::Error::other(
                            "REPL line exceeds 128 KiB; use a file attachment or headless prompt input",
                        ));
                    }
                    self.bytes.push(byte);
                    if self.interactive {
                        append(echo, &[byte])?;
                    }
                }
            }
        }
        Ok(())
    }
    fn reprint(&self, echo: &mut Vec<u8>) -> io::Result<()> {
        // A fresh line avoids assuming cell widths, soft wrapping or the cursor
        // position after concurrent engine output. Erase removes one codepoint.
        let required = b"\r\nrw> ".len().saturating_add(self.bytes.len());
        if echo.len().saturating_add(required) > MAX_ECHO_BYTES {
            return Err(io::Error::other(
                "REPL terminal output is congested; input was refused",
            ));
        }
        append(echo, b"\r\nrw> ")?;
        append(echo, &self.bytes)
    }
    fn erase(&mut self) -> bool {
        let Some(last) = self.bytes.pop() else {
            return false;
        };
        if last & 0xc0 == 0x80 {
            while self.bytes.last().is_some_and(|byte| byte & 0xc0 == 0x80) {
                self.bytes.pop();
            }
            self.bytes.pop();
        }
        true
    }
    fn submit(&mut self, sender: &InputSender) -> io::Result<()> {
        let line = std::str::from_utf8(&self.bytes)
            .map_err(|_| io::Error::other("REPL input must be valid UTF-8"))?;
        let pending = sender.admit_text(line).ok_or_else(|| refusal(sender))?;
        self.bytes.clear();
        pending.publish();
        Ok(())
    }
    pub fn eof(&mut self, sender: &InputSender) -> io::Result<()> {
        if !self.bytes.is_empty() {
            self.submit(sender)?;
        }
        publish(sender, InputLine::Eof)?;
        self.ended = true;
        Ok(())
    }
}
fn publish(sender: &InputSender, value: InputLine) -> io::Result<()> {
    let pending = sender.admit(value).ok_or_else(|| refusal(sender))?;
    pending.publish();
    Ok(())
}
fn append(echo: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    if echo.len().saturating_add(bytes.len()) > MAX_ECHO_BYTES {
        return Err(io::Error::other(
            "REPL terminal output is congested; input was refused",
        ));
    }
    echo.extend_from_slice(bytes);
    Ok(())
}

fn refusal(sender: &InputSender) -> io::Error {
    io::Error::other(
        sender
            .failure
            .message()
            .unwrap_or("REPL input receiver closed"),
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn submitted_lines_reuse_scratch_and_retain_only_exact_text() {
        let (send, mut receive) = super::super::super::input::channel();
        let mut lines = Lines::new(false);
        let scratch = lines.bytes.as_ptr();
        for text in ["", "short", "é界", "another short line"]
            .into_iter()
            .cycle()
            .take(100)
        {
            lines.bytes.extend_from_slice(text.as_bytes());
            lines.submit(&send).expect("admitted line");
            assert_eq!(lines.bytes.as_ptr(), scratch);
            assert_eq!(lines.bytes.capacity(), MAX_LINE_BYTES);
            let delivery = receive.recv().await.expect("line delivery");
            assert!(matches!(&delivery.value, InputLine::Line(delivered)
                if delivered == text && delivered.capacity() == text.len()));
        }
    }
    #[tokio::test]
    async fn erase_reprints_exact_draft_without_terminal_width_assumptions() {
        let (send, mut receive) = super::super::super::input::channel();
        let (interrupts, _) = tokio::sync::watch::channel(());
        let mut lines = Lines::new(true);
        let mut echo = Vec::with_capacity(MAX_ECHO_BYTES);
        lines
            .push("e\u{301}界\t".as_bytes(), &send, &mut echo, &interrupts)
            .expect("text");
        echo.clear();
        for expected in ["e\u{301}界", "e\u{301}", "e", ""] {
            lines
                .push(&[127], &send, &mut echo, &interrupts)
                .expect("erase codepoint");
            assert_eq!(echo, format!("\r\nrw> {expected}").as_bytes());
            echo.clear();
        }
        lines
            .push("界e\u{301}".as_bytes(), &send, &mut echo, &interrupts)
            .expect("draft");
        echo.clear();
        lines
            .push(&[21, b'\n'], &send, &mut echo, &interrupts)
            .expect("clear line");
        assert_eq!(echo, b"\r\nrw> \r\nrw> ");
        assert!(
            matches!(receive.recv().await.expect("empty line").value, InputLine::Line(text) if text.is_empty())
        );
    }
}
