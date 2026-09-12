//! Fixed-size supervision protocol; no effect PID crosses this boundary.
use std::{
    io::{self, Read as _, Write as _},
    net::Shutdown,
    os::unix::net::UnixStream,
};
pub(super) const DONE: &[u8; 8] = b"RWPDONE1";

/// Owns launch permission and the supervisor's physical-retirement receipt.
/// Dropping it signals parent loss; ordinary cancellation retains the read half.
pub struct PluginLifeline {
    stream: UnixStream,
    granted: bool,
    verified: bool,
    failed: bool,
}
impl PluginLifeline {
    pub(super) fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            granted: false,
            verified: false,
            failed: false,
        })
    }
    /// Grant the accepted supervisor permission to start its sandbox child.
    /// # Errors
    /// Rejects a repeated or failed grant. A failed write is conservatively
    /// considered a possible launch and still requires a retirement receipt.
    pub fn grant(&mut self) -> io::Result<()> {
        if self.granted {
            return Err(io::Error::other("plugin launch already granted"));
        }
        self.granted = true;
        self.stream.write_all(&[1])
    }
    /// Close the parent's write half while retaining the retirement channel.
    /// # Errors
    /// Returns unexpected socket shutdown failures.
    pub fn stop(&self) -> io::Result<()> {
        match self.stream.shutdown(Shutdown::Write) {
            Ok(()) => Ok(()),
            Err(cause) if cause.kind() == io::ErrorKind::NotConnected => Ok(()),
            Err(cause) => Err(cause),
        }
    }
    /// Verify completion after the caller has reaped its supervisor.
    /// # Errors
    /// Missing, malformed, or truncated receipts remain sticky failures. A
    /// supervisor that was never granted launch cannot have started effects.
    pub fn verify_settlement(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("plugin retirement receipt unavailable"));
        }
        if !self.granted || self.verified {
            return Ok(());
        }
        let mut frame = [0; 8];
        let mut extra = [0];
        let valid = self.stream.read_exact(&mut frame).is_ok()
            && &frame == DONE
            && matches!(self.stream.read(&mut extra), Ok(0));
        if !valid {
            self.failed = true;
            return Err(io::Error::other(
                "plugin supervisor exited without retirement proof",
            ));
        }
        self.verified = true;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn absent_truncated_and_invalid_receipts_never_release_authority() {
        for bytes in [
            &b""[..],
            &DONE[..3],
            &b"invalid!"[..],
            &b"RWPDONE1extra"[..],
        ] {
            let (host, mut helper) = UnixStream::pair().expect("pair");
            let mut control = PluginLifeline::new(host).expect("owner");
            control.grant().expect("grant");
            helper.read_exact(&mut [0]).expect("consume grant");
            helper.write_all(bytes).expect("receipt bytes");
            drop(helper);
            assert!(control.verify_settlement().is_err());
            assert!(control.verify_settlement().is_err(), "failure is sticky");
        }
    }
    #[test]
    fn cancellation_preserves_receipt_and_ungranted_close_needs_no_receipt() {
        let (host, mut helper) = UnixStream::pair().expect("pair");
        let mut control = PluginLifeline::new(host).expect("owner");
        control.grant().expect("grant");
        helper.read_exact(&mut [0]).expect("consume grant");
        control.stop().expect("half close");
        helper.write_all(DONE).expect("retirement receipt");
        drop(helper);
        control.verify_settlement().expect("retired");
        control.verify_settlement().expect("idempotent");
        let (host, helper) = UnixStream::pair().expect("pair");
        let mut control = PluginLifeline::new(host).expect("owner");
        drop(helper);
        control
            .verify_settlement()
            .expect("no grant means no effect");
    }
}
