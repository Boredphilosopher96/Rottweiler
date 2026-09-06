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
                        append(echo, b"\x08 \x08")?;
                    }
                }
                21 if self.interactive => {
                    while self.erase() {
                        append(echo, b"\x08 \x08")?;
                    }
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
        let bytes = std::mem::replace(&mut self.bytes, Vec::with_capacity(MAX_LINE_BYTES));
        let line = String::from_utf8(bytes)
            .map_err(|_| io::Error::other("REPL input must be valid UTF-8"))?;
        publish(sender, InputLine::Line(line))
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
    let pending = sender.admit(value).ok_or_else(|| {
        io::Error::other(
            sender
                .failure
                .message()
                .unwrap_or("REPL input receiver closed"),
        )
    })?;
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
