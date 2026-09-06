//! Native startup readiness with captured failure evidence and owned teardown.
use super::*;
use std::process::Child;

pub(super) struct TestProcess {
    pub child: Child,
    stderr: tempfile::NamedTempFile,
}

impl TestProcess {
    pub fn spawn(command: &mut Command) -> Self {
        let stderr = tempfile::NamedTempFile::new().expect("child diagnostics");
        let child = command
            .stderr(stderr.reopen().expect("diagnostic descriptor"))
            .spawn()
            .expect("native child");
        Self { child, stderr }
    }

    pub fn wait_ready(&mut self, ready: impl Fn() -> bool) {
        // Functional tests may run many unoptimized native images concurrently.
        // Product startup latency is independently measured on release candidates.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if ready() {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("child status") {
                panic!(
                    "native child exited before readiness: {status}; stderr: {}",
                    self.diagnostics()
                );
            }
            assert!(
                Instant::now() < deadline,
                "native child never became ready; stderr: {}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn diagnostics(&self) -> String {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut file = self.stderr.reopen().expect("diagnostic reader");
        let bytes = file.metadata().expect("diagnostic size").len();
        file.seek(SeekFrom::Start(bytes.saturating_sub(64 * 1024)))
            .expect("diagnostic tail");
        let mut tail = Vec::new();
        file.take(64 * 1024)
            .read_to_end(&mut tail)
            .expect("diagnostic text");
        String::from_utf8_lossy(&tail).into_owned()
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
