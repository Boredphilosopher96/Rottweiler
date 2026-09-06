//! Native startup readiness with captured failure evidence and owned teardown.
use super::*;
use std::process::Child;

pub(super) struct TestProcess {
    pub child: Child,
    stderr: tempfile::NamedTempFile,
    journal_home: Option<PathBuf>,
}

impl TestProcess {
    pub fn spawn(command: &mut Command) -> Self {
        let stderr = tempfile::NamedTempFile::new().expect("child diagnostics");
        let journal_home = command
            .get_envs()
            .find(|(key, _)| *key == "ROTTWEILER_HOME")
            .and_then(|(_, value)| value.map(PathBuf::from));
        let child = command
            .stderr(stderr.reopen().expect("diagnostic descriptor"))
            .spawn()
            .expect("native child");
        Self {
            child,
            stderr,
            journal_home,
        }
    }

    pub fn wait_ready(&mut self, ready: impl Fn() -> bool) {
        // Functional tests may run many unoptimized native images concurrently.
        // Product startup latency is independently measured on release candidates.
        self.wait_ready_within(Duration::from_secs(30), ready);
    }

    pub fn wait_ready_within(&mut self, timeout: Duration, ready: impl Fn() -> bool) {
        let started = Instant::now();
        let deadline = started + timeout;
        loop {
            if ready() {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("child status") {
                panic!(
                    "native child {} exited before readiness after {:?}: {status}; stderr: {}",
                    self.child.id(),
                    started.elapsed(),
                    self.diagnostics()
                );
            }
            assert!(
                Instant::now() < deadline,
                "native child {} never became ready within {timeout:?}; stderr: {}",
                self.child.id(),
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn diagnostics(&self) -> String {
        let stderr = file_tail(self.stderr.path()).unwrap_or_else(|error| error.to_string());
        let source = self
            .journal_home
            .as_deref()
            .and_then(event_log)
            .map_or_else(
                || "no session journal published".to_owned(),
                |path| file_tail(&path).unwrap_or_else(|error| error.to_string()),
            );
        format!("{stderr}; durable journal tail: {source}")
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn file_tail(path: &Path) -> std::io::Result<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let bytes = file.metadata()?.len();
    file.seek(SeekFrom::Start(bytes.saturating_sub(64 * 1024)))?;
    let mut tail = Vec::new();
    file.take(64 * 1024).read_to_end(&mut tail)?;
    Ok(String::from_utf8_lossy(&tail).into_owned())
}

#[test]
fn readiness_failure_diagnostics_include_bounded_canonical_tool_error() {
    let root = tempdir().expect("diagnostic root");
    let home = root.path().join("home");
    let journal = home.join("sessions/fixture/journal");
    std::fs::create_dir_all(&journal).expect("journal directory");
    let mut bytes = vec![b'x'; 128 * 1024];
    bytes.extend_from_slice(
        b"\n{\"type\":\"tool_call_finished\",\"output\":\"specific launch rejection\"}\n",
    );
    std::fs::write(journal.join("active.jsonl"), bytes).expect("journal");
    let mut command = Command::new("sh");
    command
        .args(["-c", "printf 'script exhausted' >&2"])
        .env("ROTTWEILER_HOME", &home);
    let mut child = TestProcess::spawn(&mut command);
    child.child.wait().expect("diagnostic child exit");
    let evidence = child.diagnostics();
    assert!(evidence.contains("script exhausted"));
    assert!(evidence.contains("specific launch rejection"));
    assert!(evidence.len() < 65 * 1024);
}
