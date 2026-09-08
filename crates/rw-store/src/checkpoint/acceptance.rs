//! Optimized checkpoint preparation costs; fixture creation is outside timing.
use super::{CheckpointError, CheckpointFileState, CheckpointOperation, CheckpointStore};
use rw_resources::process::BlockingProcess;
use serde_json::json;
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn git(workspace: &Path, arguments: &[&str]) -> Result<()> {
    let mut process = BlockingProcess::spawn(
        Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Checkpoint Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Checkpoint Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .stdin(Stdio::null())
            .stdout(Stdio::null()),
    )?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = process.try_status()? {
            process.settle();
            assert!(
                status.success(),
                "fixture Git command failed: {arguments:?}"
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("fixture Git deadline".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn open(root: &Path, workspace: &Path) -> Result<CheckpointStore> {
    Ok(CheckpointStore::open(
        &root.join("storage"),
        workspace,
        super::CheckpointBlobStore::open(root, workspace)?,
    )?)
}

#[test]
#[ignore = "optimized large-file and monorepo checkpoint preparation measurement"]
fn qualify_checkpoint_preparation() -> Result<()> {
    let mut samples = Vec::new();
    for files in [100, 1_000, 10_000] {
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace)?;
        for index in 0..files {
            let mut content = vec![b'x'; 4096];
            content[..8].copy_from_slice(&(index as u64).to_le_bytes());
            fs::write(workspace.join(format!("source-{index}.bin")), content)?;
        }
        git(&workspace, &["init", "--quiet", "--initial-branch=main"])?;
        git(&workspace, &["add", "--", "."])?;
        git(
            &workspace,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "Fixture",
            ],
        )?;
        for index in 0..8 {
            fs::write(workspace.join(format!("source-{index}.bin")), [b'd'; 4096])?;
        }
        // Generated untracked data participates in the actual inventory policy.
        fs::create_dir(workspace.join("generated"))?;
        for index in 0..100 {
            fs::write(
                workspace.join(format!("generated/{index}.bin")),
                [b'g'; 4096],
            )?;
        }
        let store = open(root.path(), &workspace)?;
        for sample in 0..3 {
            let mut operation = CheckpointOperation::default();
            let started = Instant::now();
            let mutation = store.begin_opaque_mutation("capture", sample + 1, &mut operation)?;
            let preparation_us = started.elapsed().as_micros();
            // This mutation occurs after the captured baseline, outside timing.
            let external = workspace.join("source-8.bin");
            let before = fs::read(&external)?;
            fs::write(&external, [b'e'; 4096])?;
            let finish = Instant::now();
            let manifest = store.finish_opaque_mutation(&mutation, &mut operation)?;
            let finish_us = finish.elapsed().as_micros();
            assert_eq!(manifest.files.len(), 9);
            let CheckpointFileState::Present { blob, bytes, .. } = &manifest.files["source-8.bin"]
            else {
                return Err("tracked external mutation lost its preimage".into());
            };
            assert_eq!(store.read_valid_blob(blob, *bytes)?, before);
            fs::write(&external, before)?;
            samples.push(json!({"files": files, "bytes_per_file": 4096,
                "generated_files": 100, "dirty_tracked_files": 8, "sample": sample,
                "command_preparation_us": preparation_us, "finish_us": finish_us}));
        }
    }
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace)?;
    let sparse = workspace.join("sparse.bin");
    File::create(&sparse)?.set_len(super::MAX_CAPTURE_FILE_BYTES)?;
    let store = open(root.path(), &workspace)?;
    let started = Instant::now();
    let manifest = store.checkpoint_known(
        "large",
        1,
        [PathBuf::from("sparse.bin")],
        &mut CheckpointOperation::default(),
    )?;
    let capture_us = started.elapsed().as_micros();
    let CheckpointFileState::Present { blob, bytes, .. } = &manifest.files["sparse.bin"] else {
        return Err("large preimage missing".into());
    };
    assert_eq!(*bytes, super::MAX_CAPTURE_FILE_BYTES);
    // Stream the oracle too: do not inflate this process's peak with a 64 MiB Vec.
    let mut saved = File::open(store.blobs.directory().join(&blob[..2]).join(blob))?;
    let mut chunk = [0; super::CAPTURE_CHUNK_BYTES];
    let mut observed = 0_u64;
    loop {
        let count = saved.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        assert!(chunk[..count].iter().all(|byte| *byte == 0));
        observed += count as u64;
    }
    assert_eq!(observed, *bytes);
    File::options()
        .write(true)
        .open(&sparse)?
        .set_len(1024 * 1024 * 1024)?;
    let refusal = Instant::now();
    assert!(matches!(
        store.checkpoint_known(
            "large",
            2,
            [PathBuf::from("sparse.bin")],
            &mut CheckpointOperation::default()
        ),
        Err(CheckpointError::CaptureFileLimit)
    ));
    let refusal_us = refusal.elapsed().as_micros();
    assert!(!store.manifest_path("large", 2).exists());
    let mut cancelled = CheckpointOperation::default();
    cancelled.cancellation().cancel();
    assert!(matches!(
        store.begin_opaque_mutation("cancelled", 1, &mut cancelled),
        Err(CheckpointError::Cancelled)
    ));
    assert!(!store.pending_path("cancelled", 1).exists());
    println!(
        "{}",
        json!({"checkpoint_preparation": samples,
        "sparse_bytes": super::MAX_CAPTURE_FILE_BYTES, "sparse_capture_us": capture_us,
        "oversized_bytes": 1024 * 1024 * 1024_u64, "oversized_refusal_us": refusal_us,
        "capture_chunk_bytes": super::CAPTURE_CHUNK_BYTES,
        "physical_read_bytes": null, "cache_state": "fresh fixture; OS cache not evicted"})
    );
    Ok(())
}
