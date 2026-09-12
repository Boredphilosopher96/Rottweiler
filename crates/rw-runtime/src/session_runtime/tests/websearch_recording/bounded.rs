use super::{
    ReplayingConfiguredWebSearcher, WEBSEARCH_REPLAY_FILE, WebSearchFixtureDirectory, tempdir,
};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempdir().expect("fixture directory");
    let path = directory.path().join(WEBSEARCH_REPLAY_FILE);
    std::fs::write(&path, br#"{"fixture":[]}"#).expect("fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("private fixture");
    }
    (directory, path)
}

#[test]
fn oversized_fixture_rejected_before_read_allocation() {
    let (directory, path) = fixture();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("file")
        .set_len(rw_providers::MAX_RECORDING_FIXTURE_BYTES as u64 + 1)
        .expect("sparse oversize");
    let owner = WebSearchFixtureDirectory::open(directory.path(), false).expect("directory");
    let Err(error) = owner.open_fixture() else {
        panic!("oversized descriptor admitted")
    };
    assert!(error.to_string().contains("recording limit"));
}

#[test]
fn pinned_fixture_rejects_growth_and_truncation() {
    for length in [2, 128] {
        let (directory, path) = fixture();
        let owner = WebSearchFixtureDirectory::open(directory.path(), false).expect("directory");
        let pinned = owner.open_fixture().expect("open").expect("present");
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("file")
            .set_len(length)
            .expect("mutate same descriptor");
        let error = pinned.read().expect_err("changed file rejected");
        assert!(error.to_string().contains("changed"));
    }
}

#[test]
fn bounded_reader_rejects_malformed_occurrence_arrays() {
    let (directory, path) = fixture();
    for bytes in [
        b"{\"fixture\":null}".as_slice(),
        b"{\"fixture\":{}}",
        b"{\"fixture\":[",
    ] {
        std::fs::write(&path, bytes).expect("malformed fixture");
        let Err(error) = ReplayingConfiguredWebSearcher::load(directory.path()) else {
            panic!("malformed occurrences accepted")
        };
        assert!(error.to_string().contains("occurrences could not parse"));
    }
}

#[cfg(unix)]
#[test]
fn fifo_fixture_rejected_without_waiting_for_a_writer() {
    let directory = tempdir().expect("directory");
    let path = directory.path().join(WEBSEARCH_REPLAY_FILE);
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo")
            .success()
    );
    let owner = WebSearchFixtureDirectory::open(directory.path(), false).expect("directory");
    let (done, result) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = done.send(owner.open_fixture().map(|_| ()));
    });
    let result = result.recv_timeout(std::time::Duration::from_secs(2));
    // Settle the negative case too: a blocking open needs a writer before join.
    let release = if result.is_err() {
        Some(
            rustix::fs::open(
                &path,
                rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            )
            .expect("release blocked FIFO open"),
        )
    } else {
        None
    };
    worker.join().expect("reader settled");
    drop(release);
    let error = result
        .expect("FIFO rejection cannot wait for a writer")
        .expect_err("special file rejected");
    assert!(error.to_string().contains("regular file"));
}

#[test]
fn dense_fixture_rejected_by_decode_admission_before_typed_arrays() {
    let (directory, path) = fixture();
    let body = format!("{{\"fixture\":[{}null]}}", "null,".repeat(600_000));
    assert!(body.len() < rw_providers::MAX_RECORDING_FIXTURE_BYTES);
    std::fs::write(path, body).expect("dense fixture");
    let Err(error) = ReplayingConfiguredWebSearcher::load(directory.path()) else {
        panic!("dense fixture accepted")
    };
    assert!(error.to_string().contains("decoded admission exceeded"));
}
